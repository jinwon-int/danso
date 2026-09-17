//! Restarting the unit this process lives in (`docs/unified-design.md` §6.4).
//!
//! Ported from ccc-node `bridge/core/restart_handoff.py`, whose module comment
//! states the whole problem in one sentence:
//!
//! > The bridge process must never restart its own unit directly: systemd kills
//! > the whole target cgroup, including the command that is trying to complete
//! > the restart.
//!
//! `systemctl restart danso` issued from inside `danso.service` stops the
//! cgroup, and the `systemctl` process is *in* that cgroup. It dies partway
//! through, and what happens next depends on where exactly it was — a service
//! that came back, one that did not, or one nobody can describe. So the restart
//! is handed to somebody else: [`schedule`] asks systemd for a delayed
//! transient unit, which is its own cgroup, and that unit runs the restart and
//! writes down how it went.
//!
//! ## Fail-closed, unlike the idle gate
//!
//! [`crate::idle`] fails open because not being able to tell whether a turn is
//! running must not stop updates forever. This module is the opposite: a
//! restart that cannot be scheduled **does not happen**, and the caller is told
//! why. There is no fallback to `systemctl restart`, `kill`, or re-exec — every
//! one of those is the bug this module exists to avoid. On a host with no
//! systemd (Termux) scheduling simply fails, which is correct: that platform
//! restarts through `service run --supervise`, not through units.
//!
//! ## The receipt
//!
//! The process that asked for the restart does not survive to see it, so the
//! answer is left in a file for its replacement: `restart-handoff.json`,
//! owner-only, one line of JSON, moved to `restart-handoff.last.json` once
//! somebody has read it.
//!
//! It is **body-free**. Identifiers, enum states, a fixed reason code, two
//! pids, timestamps. No `systemctl` output, no error text, no path beyond the
//! data directory. ccc's rule, for ccc's reason: the receipt is meant to be
//! delivered into a chat, and anything in it is published there. A cap of
//! [`MAX_RECEIPT_BYTES`] keeps that surface finite even if a future field is
//! added carelessly.
//!
//! ## The receipt is also the mutex
//!
//! There is no lock. A receipt in a terminal state blocks new requests until
//! somebody archives it — losing a result is worse than refusing a second
//! restart — and a receipt still in flight blocks them for
//! [`ACTIVE_TTL_SECONDS`], after which a stuck worker stops holding the door.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const RECEIPT_FILE: &str = "restart-handoff.json";
pub const ARCHIVE_FILE: &str = "restart-handoff.last.json";
pub const SCHEMA: &str = "danso.service.restart-handoff.v1";
pub const SCHEMA_VERSION: u64 = 1;

/// A receipt larger than this is refused on both write and read.
pub const MAX_RECEIPT_BYTES: u64 = 8192;

/// How long an unfinished request keeps blocking new ones.
pub const ACTIVE_TTL_SECONDS: i64 = 300;

/// Delay bounds for the transient timer.
///
/// A delay at all, because the caller needs to finish answering before the
/// restart lands. Bounded below because zero would race that answer, and above
/// because a restart nobody is waiting for any more is not a restart, it is a
/// surprise. ccc clamps the same way, in addition to validating at config load
/// — the second clamp is what holds when a caller bypasses the first.
pub const MIN_DELAY_SECONDS: u64 = 5;
pub const MAX_DELAY_SECONDS: u64 = 30;
pub const DEFAULT_DELAY_SECONDS: u64 = 5;

/// How long the worker waits for the replacement to publish health.
pub const HEALTH_DEADLINE_SECONDS: i64 = 60;

/// The timer's permitted lateness, in seconds.
///
/// systemd defaults to one minute and coalesces timers within it to save
/// wakeups. That default is wrong here: this timer fires once, seconds from
/// now, and the caller has already told somebody when to expect it.
pub const TIMER_ACCURACY_SECONDS: u64 = 1;

/// The longest a caller should wait for a scheduled restart to resolve.
///
/// The delay, plus what systemd may add to it, plus the worker's own health
/// deadline, plus a little. A caller that builds a budget from the delay alone
/// gives up while the restart is still coming.
pub fn wait_budget_seconds(delay_seconds: u64) -> u64 {
    delay_seconds + TIMER_ACCURACY_SECONDS + HEALTH_DEADLINE_SECONDS as u64 + 10
}

/// Absolute locations searched for the systemd tools, in order.
///
/// Not `PATH`. The scheduling caller may be a long-lived service whose
/// environment came from a unit file somebody else can edit, and the thing
/// being launched restarts a system service. ccc hardcodes `/usr/bin/…` for
/// this reason; the list exists only because that path is not universal.
const SYSTEMD_RUN_PATHS: [&str; 3] = [
    "/usr/bin/systemd-run",
    "/bin/systemd-run",
    "/usr/local/bin/systemd-run",
];
const SYSTEMCTL_PATHS: [&str; 3] = [
    "/usr/bin/systemctl",
    "/bin/systemctl",
    "/usr/local/bin/systemctl",
];

/// A body-free scheduling or worker failure.
///
/// The code is the entire payload. It reaches an operator, a log, and
/// eventually a chat message, so it is a fixed vocabulary rather than anything
/// derived from an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffError {
    /// No systemd on this host, or the binary could not be run.
    SystemdRunUnavailable,
    /// systemd refused: no bus, a unit name collision, a denied policy.
    SystemdRunRejected,
    /// A request is already in flight.
    RestartAlreadyPending,
    /// A finished request has not been read yet.
    RestartResultPending,
    /// The data directory is not a private directory this user owns.
    UnsafeDataDir,
    /// The receipt is not a private regular file this user owns.
    UnsafeReceipt,
    /// The receipt is unreadable, oversized, or not this schema.
    InvalidReceipt,
    /// The worker binary path does not name a file that can be executed.
    UnusableWorker,
}

impl HandoffError {
    pub fn code(self) -> &'static str {
        match self {
            HandoffError::SystemdRunUnavailable => "systemd_run_unavailable",
            HandoffError::SystemdRunRejected => "systemd_run_rejected",
            HandoffError::RestartAlreadyPending => "restart_already_pending",
            HandoffError::RestartResultPending => "restart_result_pending",
            HandoffError::UnsafeDataDir => "unsafe_data_dir",
            HandoffError::UnsafeReceipt => "unsafe_receipt",
            HandoffError::InvalidReceipt => "invalid_receipt",
            HandoffError::UnusableWorker => "unusable_worker",
        }
    }
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for HandoffError {}

/// Why a scheduled restart did not finish. Also a fixed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// `systemctl restart` returned non-zero.
    RestartFailed,
    /// The unit came back but nothing published health in time.
    HealthTimeout,
    /// The worker could not do its job for a reason it will not describe.
    WorkerError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Written before `systemd-run`; the timer does not exist yet.
    Prepared,
    /// The worker has taken the request and is restarting.
    Armed,
    Completed,
    Failed,
}

impl State {
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Completed | State::Failed)
    }
}

/// What the worker left behind. Counts, codes and identifiers only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub schema: String,
    pub schema_version: u64,
    pub request_id: String,
    pub state: State,
    pub unit: String,
    /// The pid that asked. The replacement must not have it.
    pub origin_pid: u32,
    pub created_at: i64,
    pub updated_at: i64,
    /// Whether the units are the user manager's rather than the system's.
    pub user_scope: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<FailureCode>,
}

impl Receipt {
    pub fn summary(&self) -> String {
        let short = self.request_id.chars().take(8).collect::<String>();
        match self.state {
            State::Prepared => format!("restart {short}: scheduled"),
            State::Armed => format!("restart {short}: in progress"),
            State::Completed => match self.new_pid {
                Some(pid) => format!("restart {short}: completed, now pid {pid}"),
                None => format!("restart {short}: completed"),
            },
            State::Failed => format!(
                "restart {short}: failed ({})",
                self.reason_code
                    .map(|code| match code {
                        FailureCode::RestartFailed => "restart_failed",
                        FailureCode::HealthTimeout => "health_timeout",
                        FailureCode::WorkerError => "worker_error",
                    })
                    .unwrap_or("worker_error")
            ),
        }
    }
}

pub fn receipt_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RECEIPT_FILE)
}

pub fn archive_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ARCHIVE_FILE)
}

/// Read the receipt, refusing anything that is not a private file we own.
///
/// `Ok(None)` means there is none, which is not an error — the ordinary state
/// of a service nobody has asked to restart.
pub fn read_receipt(data_dir: &Path) -> Result<Option<Receipt>, HandoffError> {
    let path = receipt_path(data_dir);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(HandoffError::UnsafeReceipt),
    };
    check_private_file(&metadata)?;
    if metadata.len() > MAX_RECEIPT_BYTES {
        return Err(HandoffError::InvalidReceipt);
    }
    let raw = std::fs::read(&path).map_err(|_| HandoffError::UnsafeReceipt)?;
    let receipt: Receipt =
        serde_json::from_slice(&raw).map_err(|_| HandoffError::InvalidReceipt)?;
    if receipt.schema != SCHEMA || receipt.schema_version != SCHEMA_VERSION {
        return Err(HandoffError::InvalidReceipt);
    }
    Ok(Some(receipt))
}

/// A symlink, a hard link, a file somebody else owns, or one anybody can read
/// is not a receipt — it is a way for somebody else to choose what this process
/// believes about a restart.
fn check_private_file(metadata: &std::fs::Metadata) -> Result<(), HandoffError> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(HandoffError::UnsafeReceipt);
    }
    if metadata.uid() != unsafe { libc::getuid() } {
        return Err(HandoffError::UnsafeReceipt);
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(HandoffError::UnsafeReceipt);
    }
    Ok(())
}

/// The data directory must be ours and ours alone before anything is written
/// into it: a world-writable directory lets somebody else supply the receipt.
fn check_private_dir(data_dir: &Path) -> Result<(), HandoffError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(data_dir).map_err(|_| HandoffError::UnsafeDataDir)?;
    if !metadata.is_dir() {
        return Err(HandoffError::UnsafeDataDir);
    }
    if metadata.uid() != unsafe { libc::getuid() } || metadata.mode() & 0o077 != 0 {
        return Err(HandoffError::UnsafeDataDir);
    }
    Ok(())
}

fn write_receipt(data_dir: &Path, receipt: &Receipt) -> Result<(), HandoffError> {
    check_private_dir(data_dir)?;
    let encoded = serde_json::to_vec(receipt).map_err(|_| HandoffError::InvalidReceipt)?;
    if encoded.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(HandoffError::InvalidReceipt);
    }
    let temporary = data_dir.join(format!(".{RECEIPT_FILE}.{}", std::process::id()));
    write_private(&temporary, &encoded).map_err(|_| HandoffError::UnsafeReceipt)?;
    if std::fs::rename(&temporary, receipt_path(data_dir)).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(HandoffError::UnsafeReceipt);
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_receipt(data_dir: &Path) {
    let _ = std::fs::remove_file(receipt_path(data_dir));
}

/// Move a terminal receipt aside once it has been read.
///
/// Refuses unless the request matches and the state is terminal: archiving
/// somebody else's in-flight request would lose a result nobody has seen.
pub fn archive_receipt(data_dir: &Path, request_id: &str) -> Result<bool, HandoffError> {
    let Some(receipt) = read_receipt(data_dir)? else {
        return Ok(false);
    };
    if receipt.request_id != request_id || !receipt.state.is_terminal() {
        return Ok(false);
    }
    Ok(std::fs::rename(receipt_path(data_dir), archive_path(data_dir)).is_ok())
}

/// A scheduled restart, for the caller to report and later look up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scheduled {
    pub request_id: String,
    pub transient_unit: String,
    pub delay_seconds: u64,
}

/// Where the transient unit's command comes from.
pub struct Plan<'a> {
    pub data_dir: &'a Path,
    /// The unit to restart. Constant in practice, validated anyway.
    pub unit: &'a str,
    /// The binary the transient unit will execute — the worker.
    pub worker: &'a Path,
    pub delay_seconds: u64,
    /// `None` decides from the effective uid, as ccc does.
    pub user_scope: Option<bool>,
    /// `None` resolves `systemd-run` from [`SYSTEMD_RUN_PATHS`]. Supplied only
    /// by a caller whose systemd-run is elsewhere — or, in tests, by one that
    /// needs systemd to refuse. Never a `PATH` lookup: see those constants.
    pub systemd_run: Option<&'a Path>,
}

/// Ask systemd to restart the unit, from outside the unit.
///
/// Fail-closed in both directions: if the receipt cannot be written the timer
/// is never created, and if the timer cannot be created the receipt is removed
/// again. A caller must never be left believing a restart is coming when it is
/// not, and never be left with a record of one that was not scheduled.
pub fn schedule(plan: &Plan<'_>, now: i64) -> Result<Scheduled, HandoffError> {
    check_unit(plan.unit)?;
    check_worker(plan.worker)?;
    check_private_dir(plan.data_dir)?;

    // The receipt is the mutex. A finished result nobody has read outranks a
    // new request: refusing the restart is recoverable, losing the answer to
    // the last one is not.
    // A receipt this build cannot parse is an error rather than something to
    // overwrite: it may be a newer schema describing a restart in progress,
    // and scheduling a second one on top of it is the thing the mutex exists
    // to prevent.
    if let Some(current) = read_receipt(plan.data_dir)? {
        if current.state.is_terminal() {
            return Err(HandoffError::RestartResultPending);
        }
        if now.saturating_sub(current.created_at) < ACTIVE_TTL_SECONDS {
            return Err(HandoffError::RestartAlreadyPending);
        }
        // Past the TTL the previous worker is not coming back. Take over.
    }

    let user_scope = plan
        .user_scope
        .unwrap_or_else(|| unsafe { libc::geteuid() } != 0);
    let request_id = request_id();
    let transient_unit = format!("danso-restart-{request_id}");
    let delay = plan
        .delay_seconds
        .clamp(MIN_DELAY_SECONDS, MAX_DELAY_SECONDS);

    let receipt = Receipt {
        schema: SCHEMA.to_string(),
        schema_version: SCHEMA_VERSION,
        request_id: request_id.clone(),
        state: State::Prepared,
        unit: plan.unit.to_string(),
        origin_pid: std::process::id(),
        created_at: now,
        updated_at: now,
        user_scope,
        new_pid: None,
        reason_code: None,
    };
    write_receipt(plan.data_dir, &receipt)?;

    let systemd_run = match plan.systemd_run {
        Some(path) => path.to_path_buf(),
        None => match absolute_tool(&SYSTEMD_RUN_PATHS) {
            Some(path) => path,
            None => {
                remove_receipt(plan.data_dir);
                return Err(HandoffError::SystemdRunUnavailable);
            }
        },
    };

    let argv = systemd_run_argv(plan, &transient_unit, &request_id, delay, user_scope);
    let mut command = std::process::Command::new(systemd_run);
    command
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    match command.status() {
        Ok(status) if status.success() => Ok(Scheduled {
            request_id,
            transient_unit,
            delay_seconds: delay,
        }),
        Ok(_) => {
            remove_receipt(plan.data_dir);
            Err(HandoffError::SystemdRunRejected)
        }
        Err(_) => {
            remove_receipt(plan.data_dir);
            Err(HandoffError::SystemdRunUnavailable)
        }
    }
}

/// Everything after the `systemd-run` binary itself.
///
/// Built separately so a test can read it. Two of these arguments fail
/// invisibly when they are wrong — the restart still happens, just at the wrong
/// time or leaving a failed unit behind — and an argv nobody can inspect is an
/// argv nobody notices changing.
fn systemd_run_argv(
    plan: &Plan<'_>,
    transient_unit: &str,
    request_id: &str,
    delay: u64,
    user_scope: bool,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut argv: Vec<OsString> = Vec::new();
    if user_scope {
        argv.push("--user".into());
    }
    // Nothing of systemd's own chatter belongs on the caller's stdout.
    argv.push("--quiet".into());
    // Reap the transient unit even when it fails, so failures neither
    // accumulate nor block reuse of the name.
    argv.push("--collect".into());
    argv.push(format!("--unit={transient_unit}").into());
    argv.push(format!("--on-active={delay}s").into());
    // Without this the delay is a floor, not a schedule. systemd's default
    // `AccuracySec` is **one minute**, so it batches timers and may fire up to
    // a minute late — measured on yukson 2026-09-17: a `--on-active=5s`
    // handoff landed at 18s, and `systemctl show … -p AccuracyUSec` reported
    // `1min`. A restart announced as "in 5 seconds" arriving a minute later is
    // a different event, and any budget built from the delay is wrong by the
    // accuracy window.
    argv.push(format!("--timer-property=AccuracySec={TIMER_ACCURACY_SECONDS}s").into());
    argv.push(plan.worker.into());
    argv.push("service".into());
    argv.push("restart-worker".into());
    argv.push("--data-dir".into());
    argv.push(plan.data_dir.into());
    argv.push("--request-id".into());
    argv.push(request_id.into());
    if user_scope {
        // The worker checks this against the receipt; a mismatch restarts
        // nothing rather than restarting the other manager's unit.
        argv.push("--user-scope".into());
    }
    argv
}

fn absolute_tool(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// A unit name, not an option and not a path.
fn check_unit(unit: &str) -> Result<(), HandoffError> {
    let plausible = !unit.is_empty()
        && unit.len() <= 128
        && !unit.starts_with('-')
        && unit.ends_with(".service")
        && !unit.contains('/')
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@:".contains(c));
    match plausible {
        true => Ok(()),
        // Reuse of the receipt vocabulary: an implausible unit reaching here
        // means the caller is not the caller this module expects.
        false => Err(HandoffError::UnsafeDataDir),
    }
}

/// The worker must be a real file now, because the transient unit will not be
/// able to tell anybody if it is not.
///
/// `/proc/self/exe` after a binary swap resolves to an unlinked inode and
/// readlink appends ` (deleted)`; a caller handing that string in would create
/// a timer that can only fail silently. Callers should pass the installed path
/// when one exists.
fn check_worker(worker: &Path) -> Result<(), HandoffError> {
    let plausible = worker.is_absolute()
        && worker.is_file()
        && !worker.to_string_lossy().ends_with(" (deleted)");
    match plausible {
        true => Ok(()),
        false => Err(HandoffError::UnusableWorker),
    }
}

fn request_id() -> String {
    // 64 bits from the OS, rendered as hex. A collision would have to coincide
    // with a live transient unit of the same name, which systemd refuses —
    // fail-closed, so the outcome is a refused restart, not a confused one.
    //
    // `read_exact` into a fixed buffer, never `fs::read`: `/dev/urandom` has no
    // end, so reading "the file" reads until the process is killed.
    let mut bytes = [0u8; 8];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| {
            use std::io::Read;
            file.read_exact(&mut bytes)
        })
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos() as u64)
            .unwrap_or(0);
        bytes = (((std::process::id() as u64) << 32) | nanos).to_be_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Exit code for a worker that found the request was not its own.
///
/// Distinct from failure: nothing went wrong, the request simply is not the
/// one this timer was created for, so the correct action was to restart
/// nothing. A replayed or duplicated timer must be a no-op, not a second
/// restart.
pub const EXIT_WORKER_NOT_MINE: i32 = 2;

/// What the worker needs to do its job.
pub struct WorkerPlan<'a> {
    pub data_dir: &'a Path,
    pub request_id: &'a str,
    pub user_scope: bool,
    /// Where the replacement publishes its health, for the proof below.
    pub health_path: &'a Path,
    /// `None` resolves `systemctl` from [`SYSTEMCTL_PATHS`], which is what the
    /// CLI passes. A caller supplies one only to point at a systemctl that is
    /// not in a standard location — or, in tests, at a stand-in. It is a
    /// parameter rather than a `PATH` lookup for the reason given on those
    /// constants: this launches a restart of a system service.
    pub systemctl: Option<&'a Path>,
}

/// Restart the unit and prove the replacement is serving.
///
/// Runs inside the transient unit, outside the target's cgroup. Returns the
/// process exit code rather than a `Result`, because every outcome it can have
/// is already written into the receipt and there is nobody left to read a
/// message.
///
/// **"The unit restarted" is not the proof.** ccc-node #1527 is the incident
/// where it was taken as one: systemd reported success, the generation had not
/// changed, and a failed activation went unnoticed for days. So the worker
/// requires a health document that (a) postdates the request, (b) says
/// `available`, and (c) names a *different* pid from the one that asked. A
/// restart that re-executes the same image satisfies the first two and fails
/// the third.
pub fn run_worker(plan: &WorkerPlan<'_>, mut now: impl FnMut() -> i64) -> i32 {
    let Ok(Some(mut receipt)) = read_receipt(plan.data_dir) else {
        return EXIT_WORKER_NOT_MINE;
    };
    // Every one of these is a reason to restart nothing. A timer that fires
    // twice, or one created for a request that has since been superseded,
    // must not launch a restart nobody asked for.
    if receipt.request_id != plan.request_id
        || receipt.state != State::Prepared
        || receipt.user_scope != plan.user_scope
    {
        return EXIT_WORKER_NOT_MINE;
    }
    let systemctl = match plan.systemctl {
        Some(path) => path.to_path_buf(),
        None => match absolute_tool(&SYSTEMCTL_PATHS) {
            Some(path) => path,
            None => {
                return finish(
                    plan.data_dir,
                    &mut receipt,
                    Err(FailureCode::WorkerError),
                    now(),
                );
            }
        },
    };

    receipt.state = State::Armed;
    receipt.updated_at = now();
    if write_receipt(plan.data_dir, &receipt).is_err() {
        return finish(
            plan.data_dir,
            &mut receipt,
            Err(FailureCode::WorkerError),
            now(),
        );
    }

    let restarted = systemctl_ok(
        &systemctl,
        plan.user_scope,
        &["restart", "--", &receipt.unit],
    );
    if !restarted {
        return finish(
            plan.data_dir,
            &mut receipt,
            Err(FailureCode::RestartFailed),
            now(),
        );
    }

    let deadline = now() + HEALTH_DEADLINE_SECONDS;
    while now() < deadline {
        if let Some(pid) = serving_pid(&systemctl, plan, &receipt) {
            receipt.new_pid = Some(pid);
            return finish(plan.data_dir, &mut receipt, Ok(()), now());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    finish(
        plan.data_dir,
        &mut receipt,
        Err(FailureCode::HealthTimeout),
        now(),
    )
}

/// The pid that is serving, if it satisfies every part of the proof.
fn serving_pid(systemctl: &Path, plan: &WorkerPlan<'_>, receipt: &Receipt) -> Option<u32> {
    if !systemctl_ok(
        systemctl,
        plan.user_scope,
        &["is-active", "--quiet", "--", &receipt.unit],
    ) {
        return None;
    }
    let main_pid = systemctl_output(
        systemctl,
        plan.user_scope,
        &["show", "--property=MainPID", "--value", "--", &receipt.unit],
    )?
    .trim()
    .parse::<u32>()
    .ok()?;
    if main_pid == 0 || main_pid == receipt.origin_pid {
        return None;
    }
    let raw = std::fs::read(plan.health_path).ok()?;
    let document: crate::health::HealthDocument = serde_json::from_slice(&raw).ok()?;
    // The document must postdate the request. One written a moment before the
    // restart is perfectly well-formed and describes the process that is
    // already gone. One second of slack, as ccc allows, because the two
    // timestamps come from different clocks reading the same wall clock.
    let published = chrono::DateTime::parse_from_rfc3339(&document.updated_at).ok()?;
    if published.timestamp() + 1 < receipt.created_at {
        return None;
    }
    if document.service.state != crate::status::ServiceState::Available {
        return None;
    }
    let live = document.service_pid == main_pid && document.process.pid == main_pid;
    live.then_some(main_pid)
}

/// Record the outcome and give the process its exit code.
fn finish(
    data_dir: &Path,
    receipt: &mut Receipt,
    outcome: Result<(), FailureCode>,
    at: i64,
) -> i32 {
    match outcome {
        Ok(()) => receipt.state = State::Completed,
        Err(code) => {
            receipt.state = State::Failed;
            receipt.reason_code = Some(code);
        }
    }
    receipt.updated_at = at;
    // A worker that cannot write its own result still exits with the right
    // code; the alternative is dying silently and leaving a `prepared` receipt
    // that blocks the next request for its whole TTL.
    let _ = write_receipt(data_dir, receipt);
    match outcome {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn systemctl_command(systemctl: &Path, user_scope: bool, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(systemctl);
    if user_scope {
        command.arg("--user");
    }
    command.args(args);
    command
}

fn systemctl_ok(systemctl: &Path, user_scope: bool, args: &[&str]) -> bool {
    systemctl_command(systemctl, user_scope, args)
        .stdin(std::process::Stdio::null())
        // Captured and dropped. Nothing systemd says goes into the receipt.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn systemctl_output(systemctl: &Path, user_scope: bool, args: &[&str]) -> Option<String> {
    let output = systemctl_command(systemctl, user_scope, args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        (temp, dir)
    }

    fn receipt(state: State, created_at: i64) -> Receipt {
        Receipt {
            schema: SCHEMA.to_string(),
            schema_version: SCHEMA_VERSION,
            request_id: "0123456789abcdef".to_string(),
            state,
            unit: "danso.service".to_string(),
            origin_pid: 4242,
            created_at,
            updated_at: created_at,
            user_scope: true,
            new_pid: None,
            reason_code: None,
        }
    }

    #[test]
    fn a_receipt_round_trips_and_is_owner_only() {
        let (_temp, dir) = data_dir();
        let written = receipt(State::Prepared, 100);
        write_receipt(&dir, &written).unwrap();
        assert_eq!(read_receipt(&dir).unwrap(), Some(written));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(receipt_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the receipt is owner-only");
    }

    #[test]
    fn a_receipt_anybody_could_have_written_is_refused() {
        let (_temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 100)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(receipt_path(&dir), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(read_receipt(&dir), Err(HandoffError::UnsafeReceipt));
    }

    #[test]
    fn a_symlinked_receipt_is_refused() {
        let (_temp, dir) = data_dir();
        let elsewhere = dir.join("elsewhere.json");
        std::fs::write(&elsewhere, b"{}").unwrap();
        std::os::unix::fs::symlink(&elsewhere, receipt_path(&dir)).unwrap();
        assert_eq!(
            read_receipt(&dir),
            Err(HandoffError::UnsafeReceipt),
            "a symlink lets somebody else choose what we believe about a restart"
        );
    }

    #[test]
    fn a_receipt_from_another_schema_is_not_read() {
        let (_temp, dir) = data_dir();
        let mut foreign = receipt(State::Completed, 100);
        foreign.schema_version = 99;
        write_receipt(&dir, &foreign).unwrap();
        assert_eq!(read_receipt(&dir), Err(HandoffError::InvalidReceipt));
    }

    #[test]
    fn an_oversized_receipt_is_refused() {
        let (_temp, dir) = data_dir();
        let mut huge = receipt(State::Prepared, 100);
        huge.unit = format!("{}.service", "a".repeat(MAX_RECEIPT_BYTES as usize));
        assert_eq!(
            write_receipt(&dir, &huge),
            Err(HandoffError::InvalidReceipt),
            "the published surface stays finite even when a field does not"
        );
    }

    #[test]
    fn no_receipt_is_not_an_error() {
        let (_temp, dir) = data_dir();
        assert_eq!(read_receipt(&dir).unwrap(), None);
    }

    #[test]
    fn an_unread_result_outranks_a_new_request() {
        let (_temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Completed, 100)).unwrap();
        let worker = std::env::current_exe().unwrap();
        let error = schedule(
            &Plan {
                data_dir: &dir,
                unit: "danso.service",
                worker: &worker,
                delay_seconds: DEFAULT_DELAY_SECONDS,
                user_scope: Some(true),
                systemd_run: None,
            },
            // Far past any TTL: an undelivered result blocks forever, on
            // purpose. Losing the answer is worse than refusing the request.
            100 + ACTIVE_TTL_SECONDS * 100,
        )
        .unwrap_err();
        assert_eq!(error, HandoffError::RestartResultPending);
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().request_id,
            "0123456789abcdef",
            "and the result it refused for is still there"
        );
    }

    #[test]
    fn a_request_in_flight_blocks_another_until_its_ttl() {
        let (_temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Armed, 100)).unwrap();
        let worker = std::env::current_exe().unwrap();
        let plan = Plan {
            data_dir: &dir,
            unit: "danso.service",
            worker: &worker,
            delay_seconds: DEFAULT_DELAY_SECONDS,
            user_scope: Some(true),
            systemd_run: None,
        };
        assert_eq!(
            schedule(&plan, 100 + ACTIVE_TTL_SECONDS - 1).unwrap_err(),
            HandoffError::RestartAlreadyPending
        );
        // Past the TTL the worker is not coming back, so a new request may
        // take over rather than being locked out by a corpse.
        let outcome = schedule(&plan, 100 + ACTIVE_TTL_SECONDS);
        assert_ne!(outcome, Err(HandoffError::RestartAlreadyPending));
    }

    #[test]
    fn scheduling_without_systemd_leaves_no_receipt_behind() {
        // The Termux case. Pointed at a path that is not there rather than
        // skipped on hosts that have systemd — a test that silently does
        // nothing on the developer's machine is not a test.
        let (temp, dir) = data_dir();
        let absent = temp.path().join("no-systemd-run-here");
        let worker = std::env::current_exe().unwrap();
        let error = schedule(
            &Plan {
                data_dir: &dir,
                unit: "danso.service",
                worker: &worker,
                delay_seconds: DEFAULT_DELAY_SECONDS,
                user_scope: Some(true),
                systemd_run: Some(&absent),
            },
            0,
        )
        .unwrap_err();
        assert_eq!(error, HandoffError::SystemdRunUnavailable);
        assert_eq!(
            read_receipt(&dir).unwrap(),
            None,
            "a restart that was not scheduled must leave no claim that it was"
        );
    }

    #[test]
    fn a_unit_that_is_an_option_or_a_path_is_refused() {
        let (_temp, dir) = data_dir();
        let worker = std::env::current_exe().unwrap();
        for bad in [
            "-danso.service",
            "../danso.service",
            "/etc/systemd/system/danso.service",
            "danso",
            "",
            "danso.service\nExecStart=/bin/sh",
        ] {
            let error = schedule(
                &Plan {
                    data_dir: &dir,
                    unit: bad,
                    worker: &worker,
                    delay_seconds: DEFAULT_DELAY_SECONDS,
                    user_scope: Some(true),
                    systemd_run: None,
                },
                0,
            )
            .unwrap_err();
            assert_eq!(error, HandoffError::UnsafeDataDir, "{bad:?}");
            assert_eq!(read_receipt(&dir).unwrap(), None, "{bad:?}");
        }
    }

    #[test]
    fn a_worker_path_that_cannot_be_executed_is_refused() {
        let (temp, dir) = data_dir();
        for bad in [
            temp.path().join("absent"),
            PathBuf::from(format!(
                "{} (deleted)",
                std::env::current_exe().unwrap().display()
            )),
            temp.path().to_path_buf(),
        ] {
            let error = schedule(
                &Plan {
                    data_dir: &dir,
                    unit: "danso.service",
                    worker: &bad,
                    delay_seconds: DEFAULT_DELAY_SECONDS,
                    user_scope: Some(true),
                    systemd_run: None,
                },
                0,
            )
            .unwrap_err();
            assert_eq!(error, HandoffError::UnusableWorker, "{bad:?}");
        }
    }

    #[test]
    fn a_world_readable_data_directory_is_refused() {
        let (_temp, dir) = data_dir();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            write_receipt(&dir, &receipt(State::Prepared, 1)),
            Err(HandoffError::UnsafeDataDir),
            "a directory others can write lets them supply the receipt"
        );
    }

    #[test]
    fn archiving_takes_only_a_matching_terminal_receipt() {
        let (_temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Armed, 1)).unwrap();
        assert!(
            !archive_receipt(&dir, "0123456789abcdef").unwrap(),
            "an in-flight result is not finished with"
        );

        write_receipt(&dir, &receipt(State::Completed, 1)).unwrap();
        assert!(
            !archive_receipt(&dir, "somebody-elses").unwrap(),
            "and one request must not archive another's answer"
        );
        assert!(archive_receipt(&dir, "0123456789abcdef").unwrap());
        assert_eq!(read_receipt(&dir).unwrap(), None);
        assert!(archive_path(&dir).exists());
    }

    #[test]
    fn a_request_id_is_random_and_hex() {
        let first = request_id();
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, request_id());
    }

    #[test]
    fn a_waiting_caller_budgets_for_the_timers_permitted_lateness() {
        // Measured on yukson 2026-09-17: systemd's default AccuracySec is one
        // minute, and a `--on-active=5s` handoff fired at 18s. The timer now
        // asks for 1s accuracy, and a budget that leaves the accuracy window
        // out gives up while the restart is still on its way.
        //
        // Pinned as an equality: `>` passes for a budget that dropped the
        // accuracy term and kept the slack, which is exactly the regression.
        assert_eq!(
            wait_budget_seconds(DEFAULT_DELAY_SECONDS),
            DEFAULT_DELAY_SECONDS + TIMER_ACCURACY_SECONDS + HEALTH_DEADLINE_SECONDS as u64 + 10
        );
        assert_eq!(
            wait_budget_seconds(DEFAULT_DELAY_SECONDS)
                - wait_budget_seconds(DEFAULT_DELAY_SECONDS - 1),
            1,
            "and it has to track the delay it was given"
        );
    }

    /// A `systemd-run` stand-in that refuses, or accepts and does nothing.
    fn stub_systemd_run(dir: &Path, name: &str, exit: i32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nexit {exit}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn systemd_refusing_leaves_no_claim_that_a_restart_is_coming() {
        // The rejection path: no bus, a name collision, a denied policy. The
        // caller must not be left with a `prepared` receipt, which would block
        // every later request for its whole TTL while nothing was scheduled.
        let (temp, dir) = data_dir();
        let refuses = stub_systemd_run(temp.path(), "refuses", 1);
        let worker = std::env::current_exe().unwrap();
        let error = schedule(
            &Plan {
                data_dir: &dir,
                unit: "danso.service",
                worker: &worker,
                delay_seconds: DEFAULT_DELAY_SECONDS,
                user_scope: Some(true),
                systemd_run: Some(&refuses),
            },
            0,
        )
        .unwrap_err();
        assert_eq!(error, HandoffError::SystemdRunRejected);
        assert_eq!(
            read_receipt(&dir).unwrap(),
            None,
            "a restart that was refused must leave no claim that it was armed"
        );
    }

    #[test]
    fn an_armed_restart_leaves_the_receipt_that_describes_it() {
        let (temp, dir) = data_dir();
        let accepts = stub_systemd_run(temp.path(), "accepts", 0);
        let worker = std::env::current_exe().unwrap();
        let scheduled = schedule(
            &Plan {
                data_dir: &dir,
                unit: "danso.service",
                worker: &worker,
                delay_seconds: 9,
                user_scope: Some(true),
                systemd_run: Some(&accepts),
            },
            1_000,
        )
        .unwrap();
        assert_eq!(scheduled.delay_seconds, 9);
        assert_eq!(
            scheduled.transient_unit,
            format!("danso-restart-{}", scheduled.request_id)
        );
        let receipt = read_receipt(&dir).unwrap().unwrap();
        assert_eq!(receipt.state, State::Prepared);
        assert_eq!(receipt.request_id, scheduled.request_id);
        assert_eq!(receipt.origin_pid, std::process::id());
        assert_eq!(receipt.created_at, 1_000);
        assert!(receipt.user_scope);
    }

    #[test]
    fn the_systemd_run_argv_is_exactly_this() {
        // Two of these fail invisibly. Without `--timer-property` the restart
        // still happens, up to a minute after it was announced (measured, see
        // the comment there); without `--collect` a failed transient unit is
        // left behind and the next request with that name is refused. Neither
        // shows up as a failed restart, so the argv is pinned.
        let worker = PathBuf::from("/usr/local/bin/danso");
        let dir = PathBuf::from("/srv/danso/data");
        let plan = Plan {
            data_dir: &dir,
            unit: "danso.service",
            worker: &worker,
            delay_seconds: 5,
            user_scope: Some(false),
            systemd_run: None,
        };
        let argv = systemd_run_argv(&plan, "danso-restart-abc", "abc", 5, false);
        assert_eq!(
            argv,
            [
                "--quiet",
                "--collect",
                "--unit=danso-restart-abc",
                "--on-active=5s",
                "--timer-property=AccuracySec=1s",
                "/usr/local/bin/danso",
                "service",
                "restart-worker",
                "--data-dir",
                "/srv/danso/data",
                "--request-id",
                "abc",
            ]
        );

        // User scope adds `--user` before systemd-run's own options and
        // `--user-scope` after the worker's, and the two must not be confused:
        // the first selects the manager, the second is what the worker checks
        // against the receipt.
        let user = systemd_run_argv(&plan, "danso-restart-abc", "abc", 30, true);
        assert_eq!(user.first().unwrap(), "--user");
        assert_eq!(user.last().unwrap(), "--user-scope");
        assert!(user.contains(&"--on-active=30s".into()));
    }

    #[test]
    fn the_delay_is_clamped_even_when_a_caller_bypasses_validation() {
        // Through `schedule`, not through `clamp` itself: the clamp matters
        // only if the scheduler applies it. Zero would fire before the caller
        // finished replying; an unbounded delay is a surprise, not a restart.
        let (temp, dir) = data_dir();
        let accepts = stub_systemd_run(temp.path(), "accepts", 0);
        let worker = std::env::current_exe().unwrap();
        for (asked, expected) in [
            (0, MIN_DELAY_SECONDS),
            (1, MIN_DELAY_SECONDS),
            (u64::MAX, MAX_DELAY_SECONDS),
            (9, 9),
        ] {
            let scheduled = schedule(
                &Plan {
                    data_dir: &dir,
                    unit: "danso.service",
                    worker: &worker,
                    delay_seconds: asked,
                    user_scope: Some(true),
                    systemd_run: Some(&accepts),
                },
                1_000,
            )
            .expect("armed");
            assert_eq!(scheduled.delay_seconds, expected, "asked for {asked}");
            // The receipt is the mutex, so clear it before the next round.
            remove_receipt(&dir);
        }
    }

    /// A `systemctl` stand-in whose answers the test chooses.
    ///
    /// `restart` succeeds unless `restart_fails` exists; `is-active` succeeds
    /// unless `inactive` exists; `show --property=MainPID` prints the contents
    /// of `main_pid`. Files rather than arguments, so a test can change the
    /// answer between polls the way a real restart does.
    fn stub_systemctl(dir: &Path) -> PathBuf {
        let path = dir.join("systemctl");
        let script = r#"#!/bin/sh
here="$(dirname "$0")"
for a in "$@"; do
  case "$a" in
    restart)   [ -e "$here/restart_fails" ] && exit 1; exit 0 ;;
    is-active) [ -e "$here/inactive" ] && exit 3; exit 0 ;;
    show)      cat "$here/main_pid" 2>/dev/null || echo 0; exit 0 ;;
  esac
done
exit 0
"#;
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn publish_health(path: &Path, pid: u32, at: i64, state: &str) {
        let written = chrono::DateTime::from_timestamp(at, 0)
            .unwrap()
            .to_rfc3339();
        let document = serde_json::json!({
            "schema_version": 1,
            "started_at": written,
            "last_poll_at": written,
            "active_turn_count": 0,
            "queued_counts": {},
            "service_pid": pid,
            "process": {"pid": pid, "started_at": written, "mode": "run"},
            "service": {"state": state},
            "telegram": {"state": "available", "consecutive_failures": 0},
            "workload": {
                "active_requests": 0,
                "waiting_for_turn": 0,
                "turn_occupancy": "idle",
            },
            "runtime_generation": {
                "schema": "danso.runtime-generation.v1",
                "binary_sha256": "a".repeat(64),
                "version": "9.9.9",
                "exe_path": "/usr/local/bin/danso",
                "observed_at": written,
            },
            "updated_at": written,
        });
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
        // A fixture the real type cannot read would make every worker test
        // "fail to find a serving process" for the wrong reason, and the
        // success cases would loop until the harness killed them.
        let raw = std::fs::read(path).unwrap();
        serde_json::from_slice::<crate::health::HealthDocument>(&raw)
            .expect("the fixture must be a health document this build can read");
    }

    /// A clock that advances on every read.
    ///
    /// The worker's deadline is measured from it, so a constant clock means a
    /// deadline that never arrives — the loop would run until something killed
    /// the process. Production passes real time; tests must advance too.
    fn ticking(start: i64, step: i64) -> impl FnMut() -> i64 {
        let mut clock = start;
        move || {
            let now = clock;
            clock += step;
            now
        }
    }

    fn worker_plan<'a>(
        dir: &'a Path,
        health: &'a Path,
        systemctl: &'a Path,
        request_id: &'a str,
    ) -> WorkerPlan<'a> {
        WorkerPlan {
            data_dir: dir,
            request_id,
            user_scope: true,
            health_path: health,
            systemctl: Some(systemctl),
        }
    }

    #[test]
    fn the_worker_completes_when_a_different_pid_is_serving() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 7777, 1_001, "available");

        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_002, 1),
        );
        assert_eq!(code, 0);
        let done = read_receipt(&dir).unwrap().unwrap();
        assert_eq!(done.state, State::Completed);
        assert_eq!(done.new_pid, Some(7777));
        assert_eq!(done.reason_code, None);
    }

    #[test]
    fn a_restart_that_re_executed_the_same_process_is_not_a_restart() {
        // ccc-node #1527: "the unit restarted" reported success while the
        // generation had not changed. The origin pid still serving means the
        // replacement never happened, however healthy it looks.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        // 4242 is the receipt's `origin_pid`.
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 4242, 1_001, "available");

        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_000, 30),
        );
        assert_eq!(code, 1);
        let done = read_receipt(&dir).unwrap().unwrap();
        assert_eq!(done.state, State::Failed);
        assert_eq!(done.reason_code, Some(FailureCode::HealthTimeout));
        assert_eq!(done.new_pid, None);
    }

    #[test]
    fn health_written_before_the_request_is_not_proof() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        // Fresh-looking, right pid, and written before the restart was asked
        // for: it describes the process that is already gone.
        publish_health(&health, 7777, 900, "available");

        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_000, 30),
        );
        assert_eq!(code, 1);
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().reason_code,
            Some(FailureCode::HealthTimeout)
        );
    }

    #[test]
    fn a_degraded_replacement_is_not_a_completed_restart() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 7777, 1_001, "degraded");

        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_000, 30),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_failed_restart_says_so_without_quoting_systemctl() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("restart_fails"), b"").unwrap();
        let health = temp.path().join("health.json");

        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_001, 1),
        );
        assert_eq!(code, 1);
        let done = read_receipt(&dir).unwrap().unwrap();
        assert_eq!(done.state, State::Failed);
        assert_eq!(done.reason_code, Some(FailureCode::RestartFailed));
        let rendered = serde_json::to_string(&done).unwrap();
        assert!(
            !rendered.contains("systemctl") && !rendered.contains("Unit"),
            "the receipt is delivered verbatim; it carries codes, not output: \
             {rendered}"
        );
    }

    #[test]
    fn a_timer_firing_for_somebody_elses_request_restarts_nothing() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");

        for wrong in ["not-the-request", ""] {
            let code = run_worker(
                &worker_plan(&dir, &health, &systemctl, wrong),
                ticking(1_001, 1),
            );
            assert_eq!(code, EXIT_WORKER_NOT_MINE, "{wrong:?}");
            assert_eq!(
                read_receipt(&dir).unwrap().unwrap().state,
                State::Prepared,
                "and the real request is untouched"
            );
        }
    }

    #[test]
    fn a_request_already_taken_is_not_taken_again() {
        // A duplicated timer must be a no-op, not a second restart.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Armed, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");
        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_001, 1)
            ),
            EXIT_WORKER_NOT_MINE
        );
    }

    #[test]
    fn a_scope_mismatch_restarts_nothing() {
        // The receipt says user scope; a worker told otherwise would restart
        // the wrong manager's unit.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");
        let mut plan = worker_plan(&dir, &health, &systemctl, "0123456789abcdef");
        plan.user_scope = false;
        assert_eq!(run_worker(&plan, ticking(1_001, 1)), EXIT_WORKER_NOT_MINE);
        assert_eq!(read_receipt(&dir).unwrap().unwrap().state, State::Prepared);
    }

    #[test]
    fn a_worker_with_no_request_at_all_does_nothing() {
        let (temp, dir) = data_dir();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");
        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "anything"),
                ticking(1, 1)
            ),
            EXIT_WORKER_NOT_MINE
        );
    }

    #[test]
    fn the_unit_not_coming_back_is_a_health_timeout_not_a_hang() {
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("inactive"), b"").unwrap();
        let health = temp.path().join("health.json");

        let started = std::time::Instant::now();
        let code = run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_000, 30),
        );
        assert_eq!(code, 1);
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().reason_code,
            Some(FailureCode::HealthTimeout)
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline is wall-clock from the injected clock, so the loop \
             must end without sleeping out the real budget"
        );
    }

    #[test]
    fn a_receipt_carries_no_prose() {
        let mut failed = receipt(State::Failed, 1);
        failed.reason_code = Some(FailureCode::RestartFailed);
        let rendered = serde_json::to_string(&failed).unwrap();
        // Every value is an identifier, an enum token, a pid or a timestamp.
        // Nothing here is derived from a subprocess's output or an error.
        assert!(rendered.contains("\"reason_code\":\"restart_failed\""));
        for forbidden in ["stderr", "stdout", "Traceback", "/proc", "error:"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
        assert!((rendered.len() as u64) < MAX_RECEIPT_BYTES);
    }
}
