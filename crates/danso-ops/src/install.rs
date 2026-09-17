//! Installing a verified release over the installed binary
//! (`docs/unified-design.md` §6.3; ccc-node #1527).
//!
//! This is the first caller of [`crate::release`]. Everything it does is
//! ordered around one invariant, inherited from #1527:
//!
//! > The pending-activation record is written **before** the binary is
//! > replaced. If it cannot be written, nothing is replaced at all.
//!
//! A replaced binary with no pending record is an update nobody can verify or
//! roll back — the restart looks successful, the generation silently did not
//! change, and the evidence that anything happened is gone. So the write is not
//! best-effort and is never skipped.
//!
//! The reverse failure is deliberately tolerated: if the record is written and
//! the swap then fails, an unresolved record is left behind and `update status`
//! exits 1 until someone resolves it. A false "something may have happened" is
//! recoverable; a silent replacement is not.
//!
//! ## Order
//!
//! 1. Take the update lock. Two applies in the same `DANSO_HOME` would
//!    otherwise interleave their staging, snapshot and record writes.
//! 2. Verify the signature over `SHA256SUMS`, then the artifact's digest
//!    against that signed list. Failure here is [`ApplyError::Verification`]
//!    and exits [`EXIT_VERIFICATION_FAILED`]. There is no bypass flag.
//! 3. Refuse to proceed past an unresolved activation for a *different*
//!    generation: stacking updates would destroy the record and the rollback
//!    target that the outstanding one still needs.
//! 4. Extract the single expected member and stage it.
//! 5. Run the staged binary's `--version`. An artifact that cannot report its
//!    own version does not get to become the installed one.
//! 6. Snapshot the current binary as the rollback target, and digest **the
//!    snapshot** so the record names the bytes that were actually kept.
//! 7. Write the pending record.
//! 8. `rename` the staged file over the target — atomic within a directory, so
//!    no reader ever observes a half-written binary.
//! 9. Append a body-free JSONL line to `state/self-update.log`. Every attempt
//!    gets one, including a refused one: "someone tried to install a release
//!    that does not verify" is the most important line the log can carry.
//!
//! ## Extraction
//!
//! `tar` is invoked as `tar -xzOf <archive> -- danso`: the member is written to
//! **stdout**, so the archive cannot create a single file on disk no matter
//! what paths it contains. This module writes the one file itself, at a path it
//! chose. That removes path traversal, symlink and hardlink attacks from the
//! extraction step by construction rather than by checking for them, and it
//! adds no dependency to a crate whose whole point is to stay small.
//!
//! The archive must contain the member exactly once, named `danso` with no
//! leading `./`. GNU tar concatenates duplicate members and does not match
//! `danso` against a stored `./danso`, so the listing is checked before the
//! extraction rather than trusting either behaviour.
//!
//! ## Files this module creates
//!
//! Every one of them is opened `O_CREAT | O_EXCL | O_NOFOLLOW`. `$DANSO_HOME`
//! may be writable by the service user while `update apply` runs as root, and
//! a pre-planted symlink at any of these paths would otherwise turn "write in
//! the bin directory" into "overwrite and chmod any file the updater can
//! reach". The state writer already takes this precaution
//! ([`crate::update`]); nothing here may be weaker than it.

use crate::release::{self, Manifest};
use crate::update::{
    self, GenerationRef, InstalledGeneration, PendingActivation, Source, UPDATE_LOCK_FILE,
    UPDATE_LOG_FILE,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `docs/unified-design.md` §6.3: verification failure exits 13, with no
/// override. A distinct code is what lets an operator or a cron wrapper tell
/// "this release is not trustworthy" from "the update could not run".
pub const EXIT_VERIFICATION_FAILED: i32 = 13;
/// Any other failure to apply.
pub const EXIT_APPLY_FAILED: i32 = 2;

/// The member name inside the release archive.
pub const ARCHIVE_MEMBER: &str = "danso";

/// Default time the staged binary gets to answer `--version`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to keep retrying a probe whose image is still held open for
/// writing by a descriptor some other thread's child inherited.
const SPAWN_RETRY_WINDOW: Duration = Duration::from_secs(5);
/// Hard cap on what the probe may write, enforced with `RLIMIT_FSIZE` so the
/// kernel stops the child at the limit instead of this module noticing
/// afterwards that a disk was filled.
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

/// What to install and where.
pub struct ApplyPlan<'a> {
    /// Directory holding `SHA256SUMS`, `SHA256SUMS.minisig` and the artifact.
    pub artifact_dir: &'a Path,
    /// Artifact file name, as listed in the signed manifest.
    pub artifact_name: &'a str,
    /// Minisign public key line trusted for this install.
    pub public_key: &'a str,
    pub danso_home: &'a Path,
    /// Units the activation expects to restart; empty for a CLI-only install.
    pub services: Vec<String>,
}

/// What an apply did.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApplyReport {
    pub version: String,
    pub target_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_sha256: Option<String>,
    /// False when the artifact was already the installed binary.
    pub replaced: bool,
}

/// Why an apply did not happen.
///
/// Split by consequence, not by call site: a verification failure means the
/// release is not to be trusted, everything else means the update could not be
/// carried out. They exit differently because operators respond differently —
/// a cron wrapper that cannot tell a mistyped path from a forged signature will
/// eventually treat both the same way.
#[derive(Debug)]
pub enum ApplyError {
    Verification(anyhow::Error),
    Failed(anyhow::Error),
}

impl ApplyError {
    pub fn exit_code(&self) -> i32 {
        match self {
            ApplyError::Verification(_) => EXIT_VERIFICATION_FAILED,
            ApplyError::Failed(_) => EXIT_APPLY_FAILED,
        }
    }

    /// A short, body-free reason for the log and for stderr.
    pub fn reason(&self) -> &'static str {
        match self {
            ApplyError::Verification(_) => "verification_failed",
            ApplyError::Failed(_) => "apply_failed",
        }
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Verification(error) | ApplyError::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Reject a name that is anything other than a plain file name.
fn check_plain_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.contains('/')
            && !name.contains('\\')
            && name != ".."
            && name != ".",
        "artifact name must be a plain file name"
    );
    Ok(())
}

/// Verify a release directory and return the artifact's bytes.
///
/// Public because verification is useful on its own — checking a download
/// before deciding to install it must not require being willing to install it.
///
/// Only the release's *contents* are judged here. Whether the directory exists
/// at all is the caller's problem, so that a mistyped path does not come back
/// as "this release is not trustworthy".
pub fn verify_release(
    artifact_dir: &Path,
    artifact_name: &str,
    public_key: &str,
) -> Result<Vec<u8>> {
    check_plain_name(artifact_name)?;
    let manifest_bytes = fs::read(artifact_dir.join(release::MANIFEST_FILE))
        .with_context(|| format!("read {}", release::MANIFEST_FILE))?;
    let signature = fs::read_to_string(artifact_dir.join(release::SIGNATURE_FILE))
        .with_context(|| format!("read {}", release::SIGNATURE_FILE))?;
    let manifest: Manifest = release::verify_manifest(&manifest_bytes, &signature, public_key)?;
    let artifact = fs::read(artifact_dir.join(artifact_name)).context("read release artifact")?;
    release::verify_artifact(&manifest, artifact_name, &artifact)?;
    Ok(artifact)
}

/// Install a verified release over `$DANSO_HOME/bin/danso`.
pub fn apply(plan: &ApplyPlan<'_>) -> Result<ApplyReport, ApplyError> {
    match apply_locked(plan) {
        Ok(report) => {
            log_event(plan.danso_home, "applied", Some(&report), None);
            Ok(report)
        }
        Err(error) => {
            log_event(
                plan.danso_home,
                error.log_event(),
                None,
                Some(error.reason()),
            );
            Err(error)
        }
    }
}

impl ApplyError {
    fn log_event(&self) -> &'static str {
        match self {
            ApplyError::Verification(_) => "refused",
            ApplyError::Failed(_) => "failed",
        }
    }
}

fn apply_locked(plan: &ApplyPlan<'_>) -> Result<ApplyReport, ApplyError> {
    // Everything that is not about trusting the release is a plain failure.
    let prelude = || -> Result<UpdateLock> {
        for service in &plan.services {
            check_unit_name(service)?;
        }
        ensure!(
            plan.artifact_dir.is_dir(),
            "artifact directory does not exist"
        );
        UpdateLock::acquire(&update::state_dir(plan.danso_home))
    };
    let lock = prelude().map_err(ApplyError::Failed)?;

    let artifact = verify_release(plan.artifact_dir, plan.artifact_name, plan.public_key)
        .map_err(ApplyError::Verification)?;
    let report = apply_verified(plan, &artifact).map_err(ApplyError::Failed)?;
    drop(lock);
    Ok(report)
}

/// A systemd unit name, not an argument.
///
/// These strings are persisted into the activation record and handed to
/// `systemctl` by the activation slice. `src/config.rs` applies the same rule
/// to `service.unit`; a name that starts with `-` is an option, not a unit.
fn check_unit_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.@:".contains(c)),
        "service must be a plain unit name"
    );
    Ok(())
}

/// Everything after the signature has been checked.
fn apply_verified(plan: &ApplyPlan<'_>, artifact: &[u8]) -> Result<ApplyReport> {
    let target = update::installed_binary(plan.danso_home);
    let bin_dir = target
        .parent()
        .context("installed binary has no parent directory")?
        .to_path_buf();
    fs::create_dir_all(&bin_dir).context("create bin directory")?;
    let state = update::state_dir(plan.danso_home);

    let staged_bytes = extract_member(&bin_dir, artifact)?;
    let target_sha256 = release::hex_digest(&staged_bytes);

    let installed_now = match read_no_follow(&target) {
        Ok(bytes) => Some(release::hex_digest(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read the installed binary"),
    };

    // Stacking a second replacement on top of an unresolved activation would
    // overwrite the record and the `danso.prev` the outstanding one still needs
    // to roll back to. `update activate` or `update rollback` resolves it; this
    // refuses rather than quietly discarding the evidence.
    if let Some(previous) = update::read_pending(&state)?
        && previous.target.binary_sha256 != target_sha256
    {
        match previous.outcome {
            update::Outcome::Pending => bail!(
                "an activation is still outstanding; resolve it before installing another release"
            ),
            // A failed activation still owns `bin/danso.prev`. Installing over
            // it would snapshot the binary that just failed on top of the
            // known-good one, destroying the rollback target at the exact
            // moment it is the only way back.
            update::Outcome::Failed => bail!(
                "the last activation failed; roll back or re-judge it before installing another release"
            ),
            update::Outcome::Activated => {}
        }
    }

    // Same directory as the target, so the final step is a rename and not a
    // cross-device copy that could be observed half-written.
    let staged = bin_dir.join(format!(".danso.staged.{}", std::process::id()));
    let guard = TempFile::new(staged.clone());
    write_new_executable(&staged, &staged_bytes).context("stage the new binary")?;

    let probe_capture = bin_dir.join(format!(".danso.probe.{}", std::process::id()));
    let version = probe_version(&staged, &probe_capture, PROBE_TIMEOUT)
        .context("staged binary failed its --version probe")?;

    if installed_now.as_deref() == Some(target_sha256.as_str()) {
        // Already installed. Doing the swap anyway would write a pending record
        // for a generation change that is not happening, and `status` would
        // then demand an activation nobody can perform. The generation record
        // is still written: a binary that is installed but unrecorded is the
        // state `status` cannot describe.
        update::write_installed(
            &state,
            &InstalledGeneration::new(version.clone(), target_sha256.clone(), Source::Release),
        )
        .context("record the installed generation")?;
        return Ok(ApplyReport {
            version,
            target_sha256,
            previous_sha256: installed_now,
            replaced: false,
        });
    }

    let snapshot = update::previous_binary(plan.danso_home);

    // Snapshot before the record so the `snapshot` field never names a file
    // that does not exist, and digest the snapshot rather than re-reading the
    // target: the record must name the bytes that were actually kept, not
    // bytes that were read at some earlier moment.
    let previous_sha256 = match installed_now {
        Some(_) => {
            let kept = copy_to_new_executable(&target, &snapshot)
                .context("snapshot the installed binary")?;
            Some(release::hex_digest(&kept))
        }
        None => None,
    };

    // The invariant. Not best-effort: a failure here returns before anything is
    // replaced, and the staged file is removed by the guard.
    let record = PendingActivation::new(
        GenerationRef {
            version: version.clone(),
            binary_sha256: target_sha256.clone(),
        },
        previous_sha256.as_ref().map(|sha| GenerationRef {
            // The outgoing version is not recoverable from the binary itself;
            // the digest is what identifies it, and the digest is what
            // activation compares.
            version: String::new(),
            binary_sha256: sha.clone(),
        }),
        plan.services.clone(),
        previous_sha256
            .is_some()
            .then(|| snapshot.display().to_string()),
    );
    update::write_pending(&state, &record).context("record the pending activation")?;

    fs::rename(&staged, &target).context("replace the installed binary")?;
    guard.released();

    let report = ApplyReport {
        version,
        target_sha256,
        previous_sha256,
        replaced: true,
    };

    if let Err(error) = update::write_installed(
        &state,
        &InstalledGeneration::new(
            report.version.clone(),
            report.target_sha256.clone(),
            Source::Release,
        ),
    ) {
        // The binary is already replaced. Losing the generation record here
        // must not also lose which generation it was, so log the identified
        // outcome before returning the failure.
        log_event(plan.danso_home, "installed_unrecorded", Some(&report), None);
        return Err(error).context("record the installed generation");
    }

    Ok(report)
}

// Stop a rollback between its two renames, so the crash window the ordering
// exists to protect can be observed. The claim "the snapshot moves first so a
// crash cannot make the record lie" is only checkable from inside that window;
// asserting the final state passes for either order. Outside tests this
// compiles to `false` and the branch disappears.
#[cfg(test)]
thread_local! {
    static STOP_AFTER_SNAPSHOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn stop_after_snapshot() -> bool {
    #[cfg(test)]
    {
        STOP_AFTER_SNAPSHOT.with(|flag| flag.replace(false))
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Swap the installed binary back to `bin/danso.prev`.
///
/// Rollback is an install in the other direction, so it obeys the same rule:
/// the pending record is written before anything is swapped, and if it cannot
/// be written nothing is swapped at all. A rollback nobody recorded is exactly
/// as unverifiable as an update nobody recorded — `status` would report the
/// forward activation as still outstanding while a different image served.
///
/// The two binaries trade places rather than one being discarded. Whatever was
/// rolled *out of* becomes the new `danso.prev`, so a rollback is itself
/// reversible; discarding it would make "roll back, discover the old one was
/// worse, roll forward" impossible without a fresh download.
pub fn rollback(danso_home: &Path, services: Vec<String>) -> Result<RollbackReport, ApplyError> {
    match rollback_inner(danso_home, services) {
        Ok(report) => {
            log_generation_event(
                danso_home,
                "rolled_back",
                Some(&report.version),
                Some(&report.target_sha256),
                Some(&report.previous_sha256),
            );
            Ok(report)
        }
        Err(error) => {
            let error = ApplyError::Failed(error);
            log_event(danso_home, "rollback_failed", None, Some(error.reason()));
            Err(error)
        }
    }
}

/// What a rollback did.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RollbackReport {
    /// The version the restored binary reports.
    pub version: String,
    /// Digest of the binary now installed.
    pub target_sha256: String,
    /// Digest of the binary that was rolled out of, now the rollback target.
    pub previous_sha256: String,
}

fn rollback_inner(danso_home: &Path, services: Vec<String>) -> Result<RollbackReport> {
    for service in &services {
        check_unit_name(service)?;
    }
    let state = update::state_dir(danso_home);
    let lock = UpdateLock::acquire(&state)?;

    // Inherit the units from the record being superseded unless the caller
    // named some. `activate` picks its evidence from this list: a rollback of a
    // service install that recorded no services would be judged by reading the
    // file the rollback itself just wrote, which proves nothing.
    let superseded = update::read_pending(&state)?;
    let services = match (services.is_empty(), &superseded) {
        (true, Some(previous)) => previous.services.clone(),
        _ => services,
    };

    let target = update::installed_binary(danso_home);
    let snapshot = update::previous_binary(danso_home);
    let bin_dir = target
        .parent()
        .context("installed binary has no parent directory")?
        .to_path_buf();

    let restored = read_no_follow(&snapshot).context("no rollback target is available")?;
    ensure!(!restored.is_empty(), "the rollback target is empty");
    let outgoing = read_no_follow(&target).context("read the installed binary")?;
    let target_sha256 = release::hex_digest(&restored);
    let previous_sha256 = release::hex_digest(&outgoing);
    ensure!(
        target_sha256 != previous_sha256,
        "the rollback target is already installed"
    );

    // Prove the old binary still runs before trusting it with the position.
    // "It used to work" is not evidence about the file as it exists now.
    let staged = bin_dir.join(format!(".danso.rollback.{}", std::process::id()));
    let staged_guard = TempFile::new(staged.clone());
    write_new_executable(&staged, &restored).context("stage the rollback target")?;
    let capture = bin_dir.join(format!(".danso.rollback-probe.{}", std::process::id()));
    let version = probe_version(&staged, &capture, PROBE_TIMEOUT)
        .context("the rollback target failed its --version probe")?;

    // Keep the outgoing binary before the swap so the new `danso.prev` is
    // never missing between the two renames.
    let keep = bin_dir.join(format!(".danso.rollback-keep.{}", std::process::id()));
    let keep_guard = TempFile::new(keep.clone());
    write_new_executable(&keep, &outgoing).context("stage the outgoing binary")?;

    // The invariant, in the other direction.
    let record = PendingActivation::new(
        GenerationRef {
            version: version.clone(),
            binary_sha256: target_sha256.clone(),
        },
        Some(GenerationRef {
            version: String::new(),
            binary_sha256: previous_sha256.clone(),
        }),
        services,
        Some(snapshot.display().to_string()),
    );
    update::write_pending(&state, &record).context("record the pending activation")?;

    // Snapshot first, then install. A crash between the two leaves
    // `danso == danso.prev == outgoing`, which is exactly what the record now
    // says: target not yet installed, previous is the outgoing image, and the
    // snapshot path holds it. `activate` then reports "not activated", which is
    // true and actionable. The other order leaves `danso.prev` still holding
    // the image that was just installed, so the record names a previous
    // generation that is nowhere on disk — a record that lies is worse than one
    // that reports unfinished work.
    fs::rename(&keep, &snapshot).context("keep the rolled-out binary")?;
    keep_guard.released();
    if stop_after_snapshot() {
        bail!("stopped between the snapshot and the install");
    }
    fs::rename(&staged, &target).context("restore the previous binary")?;
    staged_guard.released();

    update::write_installed(
        &state,
        &InstalledGeneration::new(version.clone(), target_sha256.clone(), Source::Release),
    )
    .context("record the installed generation")?;
    drop(lock);

    Ok(RollbackReport {
        version,
        target_sha256,
        previous_sha256,
    })
}

/// Extract the one expected member, to memory, without letting `tar` touch disk.
///
/// `archive` is the **verified** byte string, re-written to a private path this
/// module owns. Handing `tar` the downloaded file instead would re-read it from
/// disk after the digest check, and whoever could replace it between the two
/// reads would decide what gets installed. Verify-then-use has to use the same
/// bytes it verified.
fn extract_member(work_dir: &Path, archive: &[u8]) -> Result<Vec<u8>> {
    let path = work_dir.join(format!(".danso.archive.{}", std::process::id()));
    let guard = TempFile::new(path.clone());
    write_new_private(&path, archive).context("stage the verified archive")?;

    let listing = run_tar(&["-tzf"], &path, None)?;
    let members: Vec<&str> = std::str::from_utf8(&listing)
        .context("release archive listing is not UTF-8")?
        .lines()
        .map(str::trim_end_matches_newline)
        .filter(|line| !line.is_empty())
        .collect();
    // GNU tar concatenates duplicate members onto stdout and does not match
    // `danso` against a stored `./danso`. Checking the listing first turns both
    // into a clear refusal instead of a silently corrupt or missing install.
    let matches = members.iter().filter(|m| **m == ARCHIVE_MEMBER).count();
    ensure!(
        matches == 1,
        "release archive must contain exactly one `{ARCHIVE_MEMBER}` member, found {matches}"
    );

    let member = run_tar(&["-xzOf"], &path, Some(ARCHIVE_MEMBER))?;
    drop(guard);
    ensure!(!member.is_empty(), "extracted binary is empty");
    Ok(member)
}

trait TrimEnd {
    fn trim_end_matches_newline(&self) -> &str;
}

impl TrimEnd for str {
    fn trim_end_matches_newline(&self) -> &str {
        self.trim_end_matches(['\r', '\n'])
    }
}

/// Run `tar`, keeping its stderr out of the operator's terminal.
///
/// tar's messages name the staged archive path; this module's contract is that
/// nothing it prints carries a filesystem path.
fn run_tar(flags: &[&str], archive: &Path, member: Option<&str>) -> Result<Vec<u8>> {
    let mut command = std::process::Command::new("tar");
    command.args(flags).arg(archive);
    if let Some(member) = member {
        command.arg("--").arg(member);
    }
    let output = command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("run tar")?;
    ensure!(
        output.status.success(),
        "release archive could not be read as a gzip tar containing `{ARCHIVE_MEMBER}`"
    );
    Ok(output.stdout)
}

/// Run `<binary> --version` and return the reported version.
///
/// Output goes to a file rather than a pipe: a pipe the parent is not draining
/// blocks the child once the buffer fills, and a blocked child cannot be
/// noticed by a timeout that is waiting for it to exit. The file is created
/// `O_EXCL | O_NOFOLLOW` at a per-process path, and the child runs under
/// `RLIMIT_FSIZE` so the size cap is enforced by the kernel rather than
/// discovered afterwards.
pub(crate) fn probe_version(
    binary: &Path,
    capture_path: &Path,
    timeout: Duration,
) -> Result<String> {
    let capture = TempFile::new(capture_path.to_path_buf());
    let file = create_new_file(capture_path, 0o600).context("create probe capture")?;
    let mut command = std::process::Command::new(binary);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null());
    limit_output(&mut command, PROBE_OUTPUT_LIMIT);
    let mut child = spawn_probe(&mut command)?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().context("wait for the probe")? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("the staged binary did not answer --version in time");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    ensure!(status.success(), "the staged binary exited non-zero");

    let meta = fs::metadata(capture_path).context("probe output")?;
    ensure!(
        meta.len() <= PROBE_OUTPUT_LIMIT,
        "probe output is too large"
    );
    let text = fs::read_to_string(capture_path).context("read probe output")?;
    drop(capture);

    // `clap` renders `danso 0.1.0` on the first line, and `long_version` adds
    // more lines below it. Taking the last token of the *whole* capture would
    // record a build-info token as the version the moment anyone adds one.
    let version = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next_back())
        .context("the staged binary reported no version")?;
    ensure!(
        version.len() <= 64
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".+-_".contains(&b)),
        "the staged binary reported an unusable version"
    );
    Ok(version.to_string())
}

/// Spawn the probe, retrying while the image is still held open for writing.
///
/// `execve` fails with `ETXTBSY` while *any* process holds a write descriptor
/// to the file. This module closes its own before probing, but a child forked
/// by another thread inherits a copy of every open descriptor and keeps it
/// until its own `exec` — the same fork window that makes the update lock need
/// a retry. The descriptor is `CLOEXEC`, so the window is short and closes on
/// its own; failing the whole update because of it would mean a busy host
/// cannot install a release.
///
/// `EAGAIN` is retried for the same reason: a `fork` that could not get a
/// process slot is a transient condition, not a bad artifact.
fn spawn_probe(command: &mut std::process::Command) -> Result<std::process::Child> {
    let deadline = Instant::now() + SPAWN_RETRY_WINDOW;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) => {
                let transient = matches!(
                    error.raw_os_error(),
                    Some(code) if code == libc::ETXTBSY || code == libc::EAGAIN
                );
                if !transient || Instant::now() >= deadline {
                    return Err(error).context("run the staged binary");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Cap what a child may write, so a flooding probe is stopped by the kernel.
#[cfg(unix)]
fn limit_output(command: &mut std::process::Command, bytes: u64) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setrlimit` is a bare syscall and is safe to call between fork
    // and exec; it allocates nothing and takes no locks.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn limit_output(_command: &mut std::process::Command, _bytes: u64) {}

/// Read a file, refusing to follow a symlink at that path.
///
/// Every path this module *writes* is `O_EXCL | O_NOFOLLOW`; the paths it
/// *reads* must be too. `bin/danso.prev` is content that gets installed and
/// `bin/danso` is content that gets snapshotted, so a link planted at either
/// turns a write in the bin directory into "copy any file the updater can
/// read into a 0755 file it can".
pub(crate) fn read_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut bytes = Vec::new();
    use std::io::Read;
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Create a file that must not already exist and must not be a symlink.
fn create_new_file(path: &Path, mode: u32) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(options.open(path)?)
}

/// Write `bytes` to a path this call creates, replacing any file already there.
///
/// The unlink is what makes a pre-planted symlink harmless: it removes the
/// link, and the `O_EXCL` create that follows cannot then be redirected.
fn write_replacing(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("clear the destination"),
    }
    use std::io::Write;
    let mut file = create_new_file(path, mode)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_new_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_replacing(path, bytes, 0o600)
}

pub(crate) fn write_new_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    write_replacing(path, bytes, 0o755)
}

/// Copy `from` to `to` through this process, returning the bytes that landed.
///
/// `fs::copy` follows a symlink at the destination; this does not. Returning
/// the bytes lets the caller digest what it kept instead of re-reading the
/// source, which may no longer be the same file.
fn copy_to_new_executable(from: &Path, to: &Path) -> Result<Vec<u8>> {
    let bytes = read_no_follow(from).context("read the binary being replaced")?;
    write_new_executable(to, &bytes)?;
    Ok(bytes)
}

/// One body-free JSONL line per apply attempt.
///
/// Digests and versions only: no paths, no archive contents, no error text.
/// The log says *what generation* was involved and how it ended, which is what
/// an incident needs, and nothing that could carry a secret.
#[derive(Serialize)]
struct LogLine<'a> {
    schema: &'a str,
    at: String,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    // Idle-gate counts. Omitted on every other event, so the schema is
    // unchanged for existing readers.
    #[serde(skip_serializing_if = "Option::is_none")]
    active_requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_request_age_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waited_seconds: Option<i64>,
}

pub const UPDATE_LOG_SCHEMA: &str = "danso.self-update.log.v1";

/// Append to the log. Never fails an apply: the log is evidence, not a gate,
/// and losing the ability to write it must not be a reason to leave a
/// half-finished install behind.
pub(crate) fn log_event(
    danso_home: &Path,
    event: &str,
    report: Option<&ApplyReport>,
    reason: Option<&'static str>,
) {
    log_line(
        danso_home,
        LogLine {
            schema: UPDATE_LOG_SCHEMA,
            at: chrono::Utc::now().to_rfc3339(),
            event,
            version: report.map(|r| r.version.as_str()),
            target_sha256: report.map(|r| r.target_sha256.as_str()),
            previous_sha256: report.and_then(|r| r.previous_sha256.as_deref()),
            replaced: report.map(|r| r.replaced),
            reason,
            active_requests: None,
            oldest_request_age_seconds: None,
            waited_seconds: None,
        },
    );
}

/// Record what the idle gate decided.
///
/// Every attempt gets a line, including one that never started: a node sitting
/// several generations behind is diagnosed from this log, and a deferral that
/// leaves nothing behind makes "the service was busy every time" and "the
/// updater never ran" look identical. ccc logs the same two events
/// (`deferred reason=bridge-busy …` and `proceed reason=defer-cap-exceeded …`)
/// for the same reason.
///
/// `event` is a fixed token from [`crate::idle::Gate::log_event`] and the two
/// numbers are counts, so the line stays body-free.
pub fn log_gate(
    danso_home: &Path,
    event: &'static str,
    busy: Option<crate::idle::BusyReason>,
    waited_seconds: Option<i64>,
) {
    log_line(
        danso_home,
        LogLine {
            schema: UPDATE_LOG_SCHEMA,
            at: chrono::Utc::now().to_rfc3339(),
            event,
            version: None,
            target_sha256: None,
            previous_sha256: None,
            replaced: None,
            reason: None,
            active_requests: busy.map(|busy| busy.active),
            oldest_request_age_seconds: busy.map(|busy| busy.oldest_seconds),
            waited_seconds,
        },
    );
}

/// Log an event that names a generation but is not an apply.
///
/// The digests are the point. A line that says only `activation_failed` leaves
/// nobody able to say *which* generation failed once the record it referred to
/// has been superseded, and superseding it is the next thing an operator does.
pub(crate) fn log_generation_event(
    danso_home: &Path,
    event: &str,
    version: Option<&str>,
    target_sha256: Option<&str>,
    previous_sha256: Option<&str>,
) {
    log_line(
        danso_home,
        LogLine {
            schema: UPDATE_LOG_SCHEMA,
            at: chrono::Utc::now().to_rfc3339(),
            event,
            version,
            target_sha256,
            previous_sha256,
            replaced: None,
            reason: None,
            active_requests: None,
            oldest_request_age_seconds: None,
            waited_seconds: None,
        },
    );
}

fn log_line(danso_home: &Path, line: LogLine<'_>) {
    let Ok(mut encoded) = serde_json::to_string(&line) else {
        return;
    };
    encoded.push('\n');
    let state = update::state_dir(danso_home);
    if fs::create_dir_all(&state).is_err() {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(UPDATE_LOG_FILE))
    {
        let _ = file.write_all(encoded.as_bytes());
    }
}

/// How long to keep trying for the update lock before declaring it contended.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Exclusive `flock` over `state/update.lock`, held for the whole apply.
///
/// Two applies in one `DANSO_HOME` would otherwise interleave their staging,
/// snapshot and record writes — and the second would overwrite the first's
/// rollback target while the first was still deciding what to record.
///
/// It is *briefly* retried rather than either blocking forever or failing on
/// the first `EWOULDBLOCK`. A `flock` belongs to the open file description, so
/// a child forked by any thread between `open` and `exec` holds a copy of this
/// lock for the length of that window. The descriptor is `CLOEXEC` and the
/// window is microseconds, but an unlucky caller would otherwise be told
/// "another update is already running" when none is. Sustained contention —
/// a real second updater — still fails, and fails fast enough to be readable
/// in a cron log.
///
/// The lock is released when the file closes, including on a crash, so a
/// killed updater never leaves the next one wedged.
pub(crate) struct UpdateLock {
    #[allow(dead_code)]
    file: fs::File,
}

impl UpdateLock {
    pub(crate) fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir).context("create state directory")?;
        let path = state_dir.join(UPDATE_LOCK_FILE);
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .context("open the update lock")?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let deadline = Instant::now() + LOCK_WAIT;
            loop {
                // SAFETY: `file` owns the descriptor for the whole call.
                let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if taken == 0 {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "another update is already running"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(Self { file })
    }
}

/// Removes its path on drop unless released.
///
/// A staged binary left behind after a failure is a file that looks installable
/// and is not; on the success path the rename consumes it and there is nothing
/// to remove.
pub(crate) struct TempFile {
    path: PathBuf,
    active: bool,
}

impl TempFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    pub(crate) fn released(mut self) {
        self.active = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Turn a missing `tar` into a clear message rather than a generic io error.
pub fn tar_available() -> bool {
    std::process::Command::new("tar")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The release key an installation trusts.
pub fn trusted_public_key(override_key: Option<&str>) -> Result<String> {
    match override_key {
        Some(key) => {
            release::check_public_key(key)?;
            Ok(key.to_string())
        }
        None => Ok(release::embedded_public_key()
            .map_err(|error| anyhow!("embedded release key is unusable: {error}"))?
            .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;

    // Fixture key. Not the release key; nothing signed by it is ever published.
    const KEY: &str = "RWRDmslv0TCmdcfE0s2lpt3hpMuvKCA0wKzLgDP7W81iToI0T/bZXQC7";

    const MANIFEST: &str = concat!(
        "3419f67d0938c6f3ced7d731968f423b3e8954c4cb02f70419e4ca4f9ba0423f  danso-9.9.9-ok.tar.gz\n",
        "5bdf87420d8b39051861eb6183ea381196544f4585dfd261cf2e5d62265967a5  danso-9.9.9-nomember.tar.gz\n",
        "0acc387c05654de1e089055d806b523fb54d469fb91821dc2bbf8fed3494f256  danso-9.9.9-badexit.tar.gz\n",
    );

    const SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RURDmslv0TCmdWq04ahc+8sQuNqv40XIg2rMRopSDywhnR7CNZupS/TfRaM0Y0TL9wWJbEjxzJPSAsk7R0DeYE1opGZPxYT2RgY=\n",
        "trusted comment: danso install fixture\n",
        "/kUu/dES/lKswJ8C1tFYuQCY0LPuBT/gMyDgVNsNcqMlWpMd8yVzZZmHO0cj+DRM01RcUU9m80p47XKTf1YMBQ==\n",
    );

    /// A `danso` member that prints `danso 9.9.9`.
    const OK: &str = "H4sIAAAAAAAAA+3SsQ6CQAzG8c48RcVdemcK8XFQTHDhEg7f35PRRJ2IMfn/OnxDO3xDh37KSbZlRee+ZvGaZkeX4NFjG6J1QSxYay5qG/da3fPSz6oyp7R8uvu2/1P7XXO+TU0eq+tlTFoPz3/Q06FMXf26HAAAAAAAAAAAAAAAAADgrQdmY3OeACgAAA==";
    /// No `danso` member at all.
    const NO_MEMBER: &str = "H4sIAAAAAAAAA+3OMQrDMBBE0a19CoUcwLsLks+TBIPcRCAr949x6cJJY4zhv2aKmWJKy2OVY+liiHHNxTZV3cWiR0/mOpioabIkQQ/+tfrM7VFDkFpK29v96i/qfuuf07ufcze+cgl56s5+BAAAAAAAAAAAAAAAAAD4xxcXa+V3ACgAAA==";
    /// A `danso` member whose `--version` exits 3.
    const BAD_EXIT: &str = "H4sIAAAAAAAAA+3SsQ6CMBDG8c59ihN3uVYK8XFQSGChCa2Jjw8yaqITMSb/3w3fcDd8w3XtlKLZl66aELZcvaZqVRkXfPC189o4o05rDUZ0516be8rtLGLmGPOnu2/7P3U8lNdxKtNg+9sQpeie/yCX0zqF7R9jlrP9dUcAAAAAAAAAAAAAAAAAwLsFTyLiCgAoAAA=";

    const OK_NAME: &str = "danso-9.9.9-ok.tar.gz";

    struct Fixture {
        _dir: tempfile::TempDir,
        release: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let release = dir.path().join("release");
            let home = dir.path().join("home");
            fs::create_dir_all(&release).unwrap();
            fs::create_dir_all(&home).unwrap();
            fs::write(release.join(release::MANIFEST_FILE), MANIFEST).unwrap();
            fs::write(release.join(release::SIGNATURE_FILE), SIGNATURE).unwrap();
            for (name, blob) in [
                (OK_NAME, OK),
                ("danso-9.9.9-nomember.tar.gz", NO_MEMBER),
                ("danso-9.9.9-badexit.tar.gz", BAD_EXIT),
            ] {
                fs::write(release.join(name), B64.decode(blob).unwrap()).unwrap();
            }
            Self {
                _dir: dir,
                release,
                home,
            }
        }

        fn plan<'a>(&'a self, artifact: &'a str) -> ApplyPlan<'a> {
            ApplyPlan {
                artifact_dir: &self.release,
                artifact_name: artifact,
                public_key: KEY,
                danso_home: &self.home,
                services: vec![],
            }
        }

        fn target(&self) -> PathBuf {
            update::installed_binary(&self.home)
        }

        fn state(&self) -> PathBuf {
            update::state_dir(&self.home)
        }

        fn install_marker(&self, body: &str) -> String {
            let target = self.target();
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, body).unwrap();
            release::hex_digest(body.as_bytes())
        }

        /// Files left in `bin/` besides the binary and its snapshot.
        fn strays(&self) -> Vec<String> {
            let bin = self.home.join("bin");
            let Ok(entries) = fs::read_dir(&bin) else {
                return vec![];
            };
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name != "danso" && name != "danso.prev")
                .collect()
        }

        fn log_lines(&self) -> Vec<serde_json::Value> {
            let path = self.state().join(UPDATE_LOG_FILE);
            let Ok(text) = fs::read_to_string(path) else {
                return vec![];
            };
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).expect("each log line is JSON"))
                .collect()
        }
    }

    /// `tar` is part of the contract, not an optional convenience: a host
    /// without it cannot install a release. Skipping these tests when it is
    /// missing would let the whole suite report `ok` while enforcing nothing.
    fn require_tar() {
        assert!(
            tar_available(),
            "these tests exercise the real extraction path and need `tar`"
        );
    }

    #[test]
    fn a_verified_release_is_installed_and_recorded() {
        require_tar();
        let fixture = Fixture::new();
        let report = apply(&fixture.plan(OK_NAME)).unwrap();

        assert_eq!(report.version, "9.9.9");
        assert!(report.replaced);
        assert!(
            report.previous_sha256.is_none(),
            "nothing was installed yet"
        );
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            report.target_sha256,
            "the installed file is the artifact that was verified"
        );

        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert!(pending.is_unresolved(), "the swap is not self-activating");
        assert_eq!(pending.target.binary_sha256, report.target_sha256);
        assert!(pending.previous.is_none());

        let installed = update::read_installed(&fixture.state()).unwrap().unwrap();
        assert_eq!(installed.version, "9.9.9");
        assert_eq!(installed.source, Source::Release);
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn the_previous_binary_is_kept_and_named_in_the_record() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho old\n");

        let report = apply(&fixture.plan(OK_NAME)).unwrap();
        assert_eq!(report.previous_sha256.as_deref(), Some(old.as_str()));

        let snapshot = update::previous_binary(&fixture.home);
        assert_eq!(
            release::hex_digest(&fs::read(&snapshot).unwrap()),
            old,
            "the rollback target holds the bytes that were replaced"
        );
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(pending.previous.unwrap().binary_sha256, old);
        assert_eq!(pending.snapshot.unwrap(), snapshot.display().to_string());
    }

    #[test]
    fn reapplying_the_same_release_demands_no_new_activation() {
        require_tar();
        let fixture = Fixture::new();
        apply(&fixture.plan(OK_NAME)).unwrap();
        let first = update::read_pending(&fixture.state()).unwrap().unwrap();

        let again = apply(&fixture.plan(OK_NAME)).unwrap();
        assert!(!again.replaced, "the artifact is already installed");

        let second = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(
            first, second,
            "a no-op must not restamp the record; a fresh pending activation \
             for a generation change that did not happen is one `status` can \
             never resolve"
        );
    }

    /// The #1527 invariant, exercised rather than asserted in prose.
    #[test]
    fn a_binary_is_never_replaced_when_the_pending_record_cannot_be_written() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho old\n");
        // `state` occupied by a regular file: `create_dir_all` fails for any
        // user, so this does not depend on the test running unprivileged.
        fs::write(fixture.state(), b"not a directory").unwrap();

        let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            old,
            "the installed binary must be untouched when the record could not \
             be written"
        );
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn verification_failures_exit_13_and_change_nothing() {
        type Break = Box<dyn Fn(&Fixture)>;
        let cases: Vec<(&str, Break)> = vec![
            (
                "tampered manifest",
                Box::new(|f: &Fixture| {
                    let path = f.release.join(release::MANIFEST_FILE);
                    let mut text = fs::read_to_string(&path).unwrap();
                    text.push_str(
                        "0000000000000000000000000000000000000000000000000000000000000000  evil\n",
                    );
                    fs::write(path, text).unwrap();
                }),
            ),
            (
                "tampered artifact",
                Box::new(|f: &Fixture| {
                    let path = f.release.join(OK_NAME);
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[0] ^= 0xff;
                    fs::write(path, bytes).unwrap();
                }),
            ),
            (
                "missing signature",
                Box::new(|f: &Fixture| {
                    fs::remove_file(f.release.join(release::SIGNATURE_FILE)).unwrap();
                }),
            ),
            (
                "missing manifest",
                Box::new(|f: &Fixture| {
                    fs::remove_file(f.release.join(release::MANIFEST_FILE)).unwrap();
                }),
            ),
        ];

        for (reason, break_it) in cases {
            let fixture = Fixture::new();
            let old = fixture.install_marker("#!/bin/sh\necho old\n");
            break_it(&fixture);

            let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
            assert_eq!(
                error.exit_code(),
                EXIT_VERIFICATION_FAILED,
                "{reason} must exit 13"
            );
            assert_eq!(
                release::hex_digest(&fs::read(fixture.target()).unwrap()),
                old,
                "{reason} must leave the installed binary alone"
            );
            assert!(
                update::read_pending(&fixture.state()).unwrap().is_none(),
                "{reason} must not record an activation"
            );
            assert!(
                fixture.strays().is_empty(),
                "{reason}: {:?}",
                fixture.strays()
            );
        }
    }

    #[test]
    fn a_signature_from_another_key_exits_13() {
        let fixture = Fixture::new();
        let other = "RWST0HTBrC+WCOk/vrCjfFlPdAlZW4wvbZSOGyEKhSXIl+oY0jX4mrCz";
        let mut plan = fixture.plan(OK_NAME);
        plan.public_key = other;
        let error = apply(&plan).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_VERIFICATION_FAILED);
    }

    #[test]
    fn an_artifact_the_manifest_does_not_list_exits_13() {
        let fixture = Fixture::new();
        fs::write(fixture.release.join("danso-9.9.9-extra.tar.gz"), b"x").unwrap();
        let error = apply(&fixture.plan("danso-9.9.9-extra.tar.gz")).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_VERIFICATION_FAILED);
    }

    #[test]
    fn an_artifact_name_that_is_a_path_is_refused_as_a_name() {
        let fixture = Fixture::new();
        for name in ["../SHA256SUMS", "sub/danso.tar.gz", "..", ".", ""] {
            let error = apply(&fixture.plan(name)).unwrap_err();
            assert_eq!(
                error.exit_code(),
                EXIT_VERIFICATION_FAILED,
                "{name} must not be installed; got: {error}"
            );
            // Asserting only the exit code would pass with the name check
            // deleted: the manifest lookup rejects these names anyway, but
            // only *after* reading whatever the path pointed at. The point of
            // the guard is that the read never happens.
            assert!(
                error.to_string().contains("plain file name"),
                "{name} must be refused as a name, before any file is read; got: {error}"
            );
        }
    }

    #[test]
    fn extraction_uses_the_bytes_that_were_verified_not_the_file_on_disk() {
        require_tar();
        let fixture = Fixture::new();
        // Verification has already happened and returned these bytes. Now the
        // artifact on disk changes — the window an attacker with write access
        // to the download directory would use.
        let verified = verify_release(&fixture.release, OK_NAME, KEY).unwrap();
        fs::write(
            fixture.release.join(OK_NAME),
            B64.decode(NO_MEMBER).unwrap(),
        )
        .unwrap();

        let plan = fixture.plan(OK_NAME);
        let report = apply_verified(&plan, &verified)
            .expect("the swapped-in archive must not be the one that gets used");
        assert_eq!(report.version, "9.9.9");
        assert_eq!(
            release::hex_digest(&verified),
            "3419f67d0938c6f3ced7d731968f423b3e8954c4cb02f70419e4ca4f9ba0423f",
            "the verified bytes are the OK archive, not what is on disk now"
        );
    }

    #[test]
    fn a_signed_archive_without_the_expected_member_is_not_installed() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho old\n");
        // Correctly signed and correctly hashed — only the contents are wrong.
        let error = apply(&fixture.plan("danso-9.9.9-nomember.tar.gz")).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            old
        );
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn a_binary_that_fails_its_version_probe_is_not_installed() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho old\n");
        let error = apply(&fixture.plan("danso-9.9.9-badexit.tar.gz")).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            old,
            "an artifact that cannot report its own version must not become \
             the installed binary"
        );
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn the_log_records_outcomes_without_bodies() {
        require_tar();
        let fixture = Fixture::new();
        apply(&fixture.plan(OK_NAME)).unwrap();
        let _ = apply(&fixture.plan("danso-9.9.9-nomember.tar.gz"));

        let lines = fixture.log_lines();
        assert_eq!(lines.len(), 2, "one line per attempt");
        assert_eq!(lines[0]["event"], "applied");
        assert_eq!(lines[0]["version"], "9.9.9");
        assert_eq!(lines[1]["event"], "failed");
        assert_eq!(lines[1]["reason"], "apply_failed");

        for line in &lines {
            assert_eq!(line["schema"], UPDATE_LOG_SCHEMA);
            let rendered = line.to_string();
            assert!(
                !rendered.contains('/'),
                "the log must carry no filesystem paths: {rendered}"
            );
        }
    }

    #[test]
    fn verify_release_returns_the_bytes_it_checked() {
        let fixture = Fixture::new();
        let bytes = verify_release(&fixture.release, OK_NAME, KEY).unwrap();
        assert_eq!(
            release::hex_digest(&bytes),
            "3419f67d0938c6f3ced7d731968f423b3e8954c4cb02f70419e4ca4f9ba0423f"
        );
    }

    /// Build a gzip tar with the given members, using the real `tar`.
    fn archive(dir: &Path, members: &[(&str, &str)]) -> Vec<u8> {
        let payload = dir.join(format!("payload{}", members.len()));
        fs::create_dir_all(&payload).unwrap();
        let mut names = Vec::new();
        for (name, body) in members {
            let path = payload.join(name.trim_start_matches("./"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            names.push((*name).to_string());
        }
        let out = dir.join("a.tar.gz");
        let mut cmd = std::process::Command::new("tar");
        cmd.arg("-czf").arg(&out).arg("-C").arg(&payload);
        for name in &names {
            cmd.arg(name);
        }
        assert!(cmd.status().unwrap().success(), "build fixture archive");
        let bytes = fs::read(&out).unwrap();
        fs::remove_file(&out).unwrap();
        bytes
    }

    fn script(body: &str) -> String {
        format!("#!/bin/sh\n{body}\n")
    }

    #[test]
    fn an_archive_must_carry_the_member_exactly_once_under_its_plain_name() {
        require_tar();
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();

        // GNU tar concatenates duplicates onto stdout: without the listing
        // check this installs two binaries glued together.
        let duplicated = {
            let payload = work.join("dup");
            fs::create_dir_all(&payload).unwrap();
            fs::write(payload.join("danso"), script("echo one")).unwrap();
            let out = work.join("dup.tar.gz");
            assert!(
                std::process::Command::new("tar")
                    .arg("-czf")
                    .arg(&out)
                    .arg("-C")
                    .arg(&payload)
                    .args(["danso", "danso"])
                    .status()
                    .unwrap()
                    .success()
            );
            fs::read(&out).unwrap()
        };
        let error = extract_member(work, &duplicated).unwrap_err();
        assert!(error.to_string().contains("exactly one"), "{error}");

        // `tar -czf x.tar.gz ./danso` is the ordinary idiom and stores `./danso`,
        // which `tar -xzO -- danso` does not match. Better a clear refusal than
        // a release pipeline that silently produces uninstallable archives.
        let dotted = archive(work, &[("./danso", &script("echo hi"))]);
        let error = extract_member(work, &dotted).unwrap_err();
        assert!(error.to_string().contains("exactly one"), "{error}");

        let plain = archive(work, &[("danso", &script("echo hi"))]);
        assert!(extract_member(work, &plain).is_ok());
    }

    #[test]
    fn an_empty_member_is_not_a_binary() {
        require_tar();
        let dir = tempfile::tempdir().unwrap();
        let empty = archive(dir.path(), &[("danso", "")]);
        let error = extract_member(dir.path(), &empty).unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    /// Stage a script and probe it, without going through a signed release.
    fn probe_script(dir: &Path, body: &str) -> Result<String> {
        let bin = dir.join(format!("probe-target-{}", body.len()));
        write_new_executable(&bin, script(body).as_bytes()).unwrap();
        probe_version(
            &bin,
            &bin.with_file_name("capture"),
            Duration::from_millis(400),
        )
    }

    #[test]
    fn the_version_is_the_first_lines_last_token() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_script(dir.path(), "echo 'danso 0.1.0'").unwrap(),
            "0.1.0"
        );
        // clap's `long_version` puts build information on later lines. Taking
        // the last token of the whole capture would record `deadbeef` as the
        // generation's version.
        assert_eq!(
            probe_script(dir.path(), "echo 'danso 0.1.0'\necho 'commit deadbeef'").unwrap(),
            "0.1.0"
        );
    }

    #[test]
    fn an_unusable_version_string_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            "echo 'danso 0.1.0;rm -rf /'",
            "echo 'danso \"quoted\"'",
            &format!("echo 'danso {}'", "9".repeat(65)),
            "true",
        ] {
            let error = probe_script(dir.path(), body).unwrap_err();
            assert!(error.to_string().contains("version"), "{body} -> {error}");
        }
    }

    #[test]
    fn a_probe_that_never_exits_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let error = probe_script(dir.path(), "echo 'danso 1.0.0'; sleep 30").unwrap_err();
        assert!(error.to_string().contains("in time"), "{error}");
    }

    #[test]
    fn a_flooding_probe_is_stopped_by_the_kernel_not_by_a_later_check() {
        let dir = tempfile::tempdir().unwrap();
        // 4 MiB is well past PROBE_OUTPUT_LIMIT. With RLIMIT_FSIZE the child
        // dies on SIGXFSZ; without it the bytes land first and the size check
        // only notices afterwards.
        let error = probe_script(
            dir.path(),
            "yes 0123456789012345678901234567890123456789 | head -c 4000000",
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-zero"), "{error}");
        let capture = dir.path().join("capture");
        assert!(
            !capture.exists() || fs::metadata(&capture).unwrap().len() <= PROBE_OUTPUT_LIMIT,
            "the cap must be enforced while writing, not discovered afterwards"
        );
    }

    #[test]
    fn a_probe_capture_path_is_not_followed_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"untouched").unwrap();
        let capture = dir.path().join("capture");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &capture).unwrap();
        let bin = dir.path().join("probe-target");
        write_new_executable(&bin, script("echo 'danso 1.0.0'").as_bytes()).unwrap();

        let _ = probe_version(&bin, &capture, Duration::from_millis(400));
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"untouched",
            "a pre-planted symlink must not turn a bin-directory write into an \
             arbitrary file overwrite"
        );
    }

    #[test]
    fn the_rollback_snapshot_is_not_written_through_a_symlink() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho old\n");
        let victim = fixture.home.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, update::previous_binary(&fixture.home)).unwrap();

        apply(&fixture.plan(OK_NAME)).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
    }

    #[test]
    fn an_outstanding_activation_blocks_a_different_release() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho old\n");
        update::write_pending(
            &fixture.state(),
            &PendingActivation::new(
                GenerationRef {
                    version: "8.8.8".into(),
                    binary_sha256: "b".repeat(64),
                },
                None,
                vec![],
                None,
            ),
        )
        .unwrap();

        let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert!(error.to_string().contains("outstanding"), "{error}");
        // The record and the rollback target the outstanding activation needs
        // are still exactly as they were.
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(pending.target.version, "8.8.8");
    }

    #[test]
    fn a_missing_artifact_directory_is_an_operator_error_not_a_trust_failure() {
        let fixture = Fixture::new();
        let missing = fixture.home.join("nope");
        let plan = ApplyPlan {
            artifact_dir: &missing,
            artifact_name: OK_NAME,
            public_key: KEY,
            danso_home: &fixture.home,
            services: vec![],
        };
        let error = apply(&plan).unwrap_err();
        assert_eq!(
            error.exit_code(),
            EXIT_APPLY_FAILED,
            "13 means the release is untrustworthy; a mistyped path is not that"
        );
    }

    #[test]
    fn a_service_must_be_a_unit_name_not_an_argument() {
        let fixture = Fixture::new();
        for bad in ["--now", "a b; rm -rf /", "", "-x"] {
            let mut plan = fixture.plan(OK_NAME);
            plan.services = vec![bad.to_string()];
            let error = apply(&plan).unwrap_err();
            assert_eq!(error.exit_code(), EXIT_APPLY_FAILED, "{bad}");
            assert!(error.to_string().contains("unit name"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_refused_release_still_leaves_a_log_line() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.release.join(release::SIGNATURE_FILE)).unwrap();
        let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_VERIFICATION_FAILED);

        let lines = fixture.log_lines();
        assert_eq!(
            lines.len(),
            1,
            "an attempt to install an unverifiable \
             release is the most important line this log can carry"
        );
        assert_eq!(lines[0]["event"], "refused");
        assert_eq!(lines[0]["reason"], "verification_failed");
    }

    #[test]
    fn a_no_op_apply_still_records_the_installed_generation() {
        require_tar();
        let fixture = Fixture::new();
        apply(&fixture.plan(OK_NAME)).unwrap();
        fs::remove_file(fixture.state().join("installed-generation.json")).unwrap();

        let again = apply(&fixture.plan(OK_NAME)).unwrap();
        assert!(!again.replaced);
        let installed = update::read_installed(&fixture.state()).unwrap();
        assert!(
            installed.is_some(),
            "a binary that is installed but unrecorded is a state `status` \
             cannot describe, and re-applying must be able to repair it"
        );
    }

    #[test]
    fn a_second_apply_cannot_run_while_the_lock_is_held() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.state()).unwrap();
        let held = UpdateLock::acquire(&fixture.state()).unwrap();

        let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert!(error.to_string().contains("already running"), "{error}");
        drop(held);
        // And it works again once the lock is free.
        if tar_available() {
            assert!(apply(&fixture.plan(OK_NAME)).is_ok());
        }
    }

    #[test]
    fn a_rollback_restores_the_snapshot_and_keeps_the_outgoing_binary() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        let applied = apply(&fixture.plan(OK_NAME)).unwrap();
        assert_eq!(applied.previous_sha256.as_deref(), Some(old.as_str()));

        let report = rollback(&fixture.home, vec![]).unwrap();
        assert_eq!(report.target_sha256, old, "the old binary is back");
        assert_eq!(report.previous_sha256, applied.target_sha256);
        assert_eq!(report.version, "0.0.1");
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            old
        );
        assert_eq!(
            release::hex_digest(&fs::read(update::previous_binary(&fixture.home)).unwrap()),
            applied.target_sha256,
            "the binary that was rolled out of becomes the new rollback target, \
             so the rollback is itself reversible"
        );
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn a_rollback_records_a_pending_activation_of_its_own() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        apply(&fixture.plan(OK_NAME)).unwrap();
        // Rollback is the remedy for a bad activation, so it deliberately
        // does *not* require the outstanding one to be resolved first.
        let report = rollback(&fixture.home, vec![]).unwrap();
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert!(
            pending.is_unresolved(),
            "a rollback nobody recorded is exactly as unverifiable as an \
             update nobody recorded"
        );
        assert_eq!(pending.target.binary_sha256, report.target_sha256);
        assert_eq!(
            pending.previous.unwrap().binary_sha256,
            report.previous_sha256
        );
    }

    #[test]
    fn a_rollback_inherits_the_units_of_the_record_it_supersedes() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        let mut plan = fixture.plan(OK_NAME);
        plan.services = vec!["danso.service".into()];
        apply(&plan).unwrap();

        // No `--service` on the rollback. Dropping the list would make the
        // follow-up `activate` judge a service install by reading the file the
        // rollback itself just wrote, which proves nothing.
        rollback(&fixture.home, vec![]).unwrap();
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(pending.services, vec!["danso.service".to_string()]);
    }

    #[test]
    fn an_explicit_service_list_overrides_the_inherited_one() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        let mut plan = fixture.plan(OK_NAME);
        plan.services = vec!["danso.service".into()];
        apply(&plan).unwrap();

        rollback(&fixture.home, vec!["other.service".into()]).unwrap();
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(pending.services, vec!["other.service".to_string()]);
    }

    #[test]
    fn a_rollback_records_the_generation_it_installed() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        apply(&fixture.plan(OK_NAME)).unwrap();
        let report = rollback(&fixture.home, vec![]).unwrap();

        let installed = update::read_installed(&fixture.state()).unwrap().unwrap();
        assert_eq!(
            installed.binary_sha256, report.target_sha256,
            "without this `status` reports the rolled-out version as installed \
             for as long as the installation lives"
        );
        assert_eq!(installed.version, "0.0.1");
    }

    #[test]
    fn a_rollback_logs_the_digests_it_moved_between() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        apply(&fixture.plan(OK_NAME)).unwrap();
        let report = rollback(&fixture.home, vec![]).unwrap();

        let line = fixture
            .log_lines()
            .into_iter()
            .find(|line| line["event"] == "rolled_back")
            .expect("the rollback is logged");
        assert_eq!(line["target_sha256"], report.target_sha256);
        assert_eq!(line["previous_sha256"], report.previous_sha256);
    }

    #[test]
    fn a_crash_between_the_two_renames_leaves_a_record_that_matches_disk() {
        require_tar();
        let fixture = Fixture::new();
        let old = fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        let applied = apply(&fixture.plan(OK_NAME)).unwrap();

        STOP_AFTER_SNAPSHOT.with(|flag| flag.set(true));
        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert!(error.to_string().contains("stopped between"), "{error:#}");

        // What a crash in that window leaves behind. The record is the
        // contract, so it has to describe this exactly.
        let pending = update::read_pending(&fixture.state()).unwrap().unwrap();
        let snapshot = update::previous_binary(&fixture.home);
        assert_eq!(
            release::hex_digest(&fs::read(&snapshot).unwrap()),
            applied.target_sha256,
            "the snapshot holds the image being rolled out of"
        );
        assert_eq!(
            pending.previous.as_ref().unwrap().binary_sha256,
            applied.target_sha256,
            "and the record names that same image as `previous`; the other \
             rename order leaves `danso.prev` holding the image that was just \
             installed, so the record points at a generation that is nowhere \
             on disk"
        );
        assert_eq!(
            pending.snapshot.as_deref(),
            Some(snapshot.to_str().unwrap())
        );
        assert_eq!(pending.target.binary_sha256, old);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            applied.target_sha256,
            "the install has not happened yet, which is what `activate` will \
             report: not activated, which is true and actionable"
        );
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn a_symlinked_rollback_target_is_not_followed() {
        let fixture = Fixture::new();
        let current = fixture.install_marker("#!/bin/sh\necho 'danso 9.9.9'\n");
        let secret = fixture.home.join("secret");
        fs::write(&secret, b"#!/bin/sh\necho 'danso 0.0.1-SECRET'\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, update::previous_binary(&fixture.home)).unwrap();

        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            current,
            "a link planted in the bin directory must not decide what gets \
             installed"
        );
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn a_symlinked_installed_binary_is_not_snapshotted_through() {
        require_tar();
        let fixture = Fixture::new();
        let secret = fixture.home.join("secret");
        fs::write(&secret, b"root only\n").unwrap();
        fs::create_dir_all(fixture.home.join("bin")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, fixture.target()).unwrap();

        let error = apply(&fixture.plan(OK_NAME)).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert!(
            !update::previous_binary(&fixture.home).exists(),
            "the snapshot must not become a copy of whatever the link pointed at"
        );
    }

    #[test]
    fn a_failed_activation_blocks_an_install_that_would_destroy_its_rollback_target() {
        require_tar();
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        apply(&fixture.plan(OK_NAME)).unwrap();
        let mut record = update::read_pending(&fixture.state()).unwrap().unwrap();
        record.outcome = update::Outcome::Failed;
        update::write_pending(&fixture.state(), &record).unwrap();

        let error = apply(&fixture.plan("danso-9.9.9-badexit.tar.gz")).unwrap_err();
        assert!(
            error.to_string().contains("last activation failed"),
            "installing over a failed activation snapshots the binary that \
             just failed on top of the known-good one; {error}"
        );
        assert_eq!(
            release::hex_digest(&fs::read(update::previous_binary(&fixture.home)).unwrap()),
            release::hex_digest(b"#!/bin/sh\necho 'danso 0.0.1'\n"),
            "the known-good rollback target is untouched"
        );
    }

    /// `execve` returns `ETXTBSY` while any process holds the image open for
    /// writing. This module closes its own descriptor first, but a child
    /// forked by another thread carries a copy until its own `exec`, so on a
    /// busy host the probe can hit a file nothing is really writing to. A
    /// write handle held here reproduces that deterministically.
    #[test]
    fn a_probe_waits_out_a_write_handle_someone_else_still_holds() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("probe-target");
        write_new_executable(&binary, script("echo 'danso 4.5.6'").as_bytes()).unwrap();

        let holder = fs::OpenOptions::new().write(true).open(&binary).unwrap();
        // Without a retry the spawn fails immediately; with one it waits for
        // the descriptor to go away, which is what the fork window does.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(holder);
        });

        let version = probe_version(&binary, &dir.path().join("capture"), Duration::from_secs(5))
            .expect("a transient ETXTBSY must not fail the update");
        release.join().unwrap();
        assert_eq!(version, "4.5.6");
    }

    #[test]
    fn a_probe_gives_up_on_a_write_handle_that_never_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("probe-target");
        write_new_executable(&binary, script("echo 'danso 4.5.6'").as_bytes()).unwrap();
        let _holder = fs::OpenOptions::new().write(true).open(&binary).unwrap();

        // Held for longer than the retry window: the retry is bounded, so a
        // genuinely unusable image still fails rather than hanging the update.
        let error = probe_version(
            &binary,
            &dir.path().join("capture"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("staged binary"), "{error:#}");
    }

    #[test]
    fn a_rollback_with_no_snapshot_changes_nothing() {
        let fixture = Fixture::new();
        let only = fixture.install_marker("#!/bin/sh\necho 'danso 0.0.1'\n");
        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert!(error.to_string().contains("rollback target"), "{error}");
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            only
        );
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
    }

    #[test]
    fn a_rollback_target_that_cannot_run_is_refused() {
        let fixture = Fixture::new();
        let current = fixture.install_marker("#!/bin/sh\necho 'danso 9.9.9'\n");
        write_new_executable(
            &update::previous_binary(&fixture.home),
            b"#!/bin/sh\nexit 3\n",
        )
        .unwrap();

        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            current,
            "\"it used to work\" is not evidence about the file as it is now"
        );
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn rolling_back_to_what_is_already_installed_is_refused() {
        let fixture = Fixture::new();
        let same = "#!/bin/sh\necho 'danso 9.9.9'\n";
        fixture.install_marker(same);
        write_new_executable(&update::previous_binary(&fixture.home), same.as_bytes()).unwrap();

        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert!(error.to_string().contains("already installed"), "{error}");
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
    }

    #[test]
    fn a_rollback_service_must_be_a_unit_name() {
        let fixture = Fixture::new();
        fixture.install_marker("#!/bin/sh\necho 'danso 9.9.9'\n");
        write_new_executable(
            &update::previous_binary(&fixture.home),
            b"#!/bin/sh\necho 'danso 0.0.1'\n",
        )
        .unwrap();

        let error = rollback(&fixture.home, vec!["--now".into()]).unwrap_err();
        assert!(error.to_string().contains("unit name"), "{error}");
        assert!(update::read_pending(&fixture.state()).unwrap().is_none());
    }

    #[test]
    fn a_rollback_is_never_performed_when_its_record_cannot_be_written() {
        let fixture = Fixture::new();
        let current = fixture.install_marker("#!/bin/sh\necho 'danso 9.9.9'\n");
        write_new_executable(
            &update::previous_binary(&fixture.home),
            b"#!/bin/sh\necho 'danso 0.0.1'\n",
        )
        .unwrap();
        // The lock lives under `state/`, so create the directory first and then
        // occupy the record's own path with a directory: the lock still works,
        // the record write cannot.
        fs::create_dir_all(fixture.state()).unwrap();
        fs::create_dir_all(fixture.state().join("pending-activation.json")).unwrap();

        let error = rollback(&fixture.home, vec![]).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_APPLY_FAILED);
        assert_eq!(
            release::hex_digest(&fs::read(fixture.target()).unwrap()),
            current,
            "the swap must not happen when the record could not be written"
        );
        assert!(fixture.strays().is_empty(), "{:?}", fixture.strays());
    }

    #[test]
    fn the_trusted_key_falls_back_to_the_embedded_one_but_not_past_a_bad_override() {
        assert_eq!(
            trusted_public_key(None).unwrap(),
            release::embedded_public_key().unwrap()
        );
        assert_eq!(trusted_public_key(Some(KEY)).unwrap(), KEY);
        // A broken override must not quietly restore the embedded key: that
        // would make a failed rotation look like a successful one.
        assert!(trusted_public_key(Some("not-a-key")).is_err());
    }
}
