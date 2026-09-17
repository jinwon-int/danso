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
    /// The data directory was given as a relative path.
    ///
    /// Its own code, not [`UnsafeDataDir`]: the transient unit resolves it
    /// against `/`, so this is a specific, recoverable operator mistake rather
    /// than a permissions problem — and one an error message should name.
    ///
    /// [`UnsafeDataDir`]: HandoffError::UnsafeDataDir
    RelativeDataDir,
    /// The receipt is not a private regular file this user owns.
    UnsafeReceipt,
    /// The receipt is unreadable or not this schema.
    InvalidReceipt,
    /// The receipt is larger than the published surface is allowed to be.
    ReceiptTooLarge,
    /// The unit named is not one this module will restart.
    InvalidUnit,
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
            HandoffError::RelativeDataDir => "relative_data_dir",
            HandoffError::UnsafeReceipt => "unsafe_receipt",
            HandoffError::InvalidReceipt => "invalid_receipt",
            HandoffError::ReceiptTooLarge => "receipt_too_large",
            HandoffError::InvalidUnit => "invalid_unit",
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
    /// The pid that asked. Provenance only — **not** the completion anchor.
    ///
    /// ccc compares the replacement against this because its scheduler is the
    /// bridge process itself. Here the scheduler is usually a short-lived CLI
    /// whose pid the unit never had, so comparing against it would always
    /// succeed and the proof would mean nothing. See [`previous_pid`].
    ///
    /// [`previous_pid`]: Receipt::previous_pid
    pub origin_pid: u32,
    /// The unit's MainPID immediately before the restart, read by the worker.
    ///
    /// This is what the replacement must differ from. `None` when the unit was
    /// not running, or systemd would not say — then anything that comes up is
    /// a replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_pid: Option<u32>,
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
    // The directory is checked on every read, not only on write: a receipt
    // that is itself a private file this user owns can still have been put
    // there by somebody who could write the directory.
    check_private_dir(data_dir)?;

    // `O_NOFOLLOW` and then `fstat` on **this descriptor** — every check below
    // is about the file that was actually opened. Checking a path and then
    // opening it again leaves a window in which the name can become a symlink
    // to something else; ccc opens dirfd-relative with `O_NOFOLLOW` and fstats
    // the fd for exactly this reason.
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(receipt_path(data_dir))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // ELOOP lands here: the name is a symlink, which is not a receipt.
        Err(_) => return Err(HandoffError::UnsafeReceipt),
    };
    let metadata = file.metadata().map_err(|_| HandoffError::UnsafeReceipt)?;
    check_private_file(&metadata)?;
    // The ceiling is on the bytes actually read, not on `metadata.len()`: the
    // size of a file that can still grow is a hint, and an unbounded read of
    // whatever is at that name is how this module already killed itself once.
    let mut raw = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| HandoffError::UnsafeReceipt)?;
    if raw.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(HandoffError::ReceiptTooLarge);
    }
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
    match is_private(
        metadata.is_file(),
        metadata.nlink(),
        metadata.uid(),
        metadata.mode(),
        // SAFETY: `getuid` is always successful and has no preconditions.
        unsafe { libc::getuid() },
    ) {
        true => Ok(()),
        false => Err(HandoffError::UnsafeReceipt),
    }
}

/// The predicate, apart from the syscalls that feed it.
///
/// Separated so the owner clause can be tested. Every other clause can be
/// arranged on a real file; "a file belonging to somebody else" cannot, inside
/// a single-user test run, and an owner check no test reaches is an owner
/// check that can be deleted without anybody noticing.
fn is_private(is_file: bool, nlink: u64, uid: u32, mode: u32, expected_uid: u32) -> bool {
    is_file && nlink == 1 && uid == expected_uid && mode & 0o077 == 0
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
        return Err(HandoffError::ReceiptTooLarge);
    }
    // A random suffix, not just the pid: `create_new` on a pid-only name that
    // a crashed predecessor left behind fails for every later process that
    // recycles the pid, turning one interrupted write into a permanently
    // unwritable receipt. ccc randomises for the same reason.
    let temporary = data_dir.join(format!(".{RECEIPT_FILE}.{}", request_id()));
    let written = write_private(&temporary, &encoded)
        .and_then(|()| std::fs::rename(&temporary, receipt_path(data_dir)));
    if written.is_err() {
        // A partial write must not survive to be found later.
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
    // A relative path would be resolved by the transient unit, whose working
    // directory is `/` and not the caller's — measured on yukson 2026-09-17.
    // The worker would find no receipt, exit quietly having restarted nothing,
    // and leave the `prepared` receipt blocking every later request for its
    // whole TTL, while the caller was told a restart was scheduled.
    if !plan.data_dir.is_absolute() {
        return Err(HandoffError::RelativeDataDir);
    }
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
        previous_pid: None,
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

    // Bounded: `systemd-run` talks to the manager over D-Bus, and a caller
    // that is itself a service must not hang here. ccc allows 8 seconds.
    //
    // A run that timed out is reported as refused, and the receipt removed. It
    // may in fact have armed the timer before hanging — in which case that
    // timer fires, finds no receipt, and restarts nothing. Both halves of that
    // are fail-closed, which is the direction to be wrong in.
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            remove_receipt(plan.data_dir);
            return Err(HandoffError::SystemdRunUnavailable);
        }
    };
    match wait_bounded(&mut child, SCHEDULE_TIMEOUT) {
        Some(true) => Ok(Scheduled {
            request_id,
            transient_unit,
            delay_seconds: delay,
        }),
        _ => {
            remove_receipt(plan.data_dir);
            Err(HandoffError::SystemdRunRejected)
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
        false => Err(HandoffError::InvalidUnit),
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
/// `available`, and (c) names a MainPID **different from the one the unit had
/// immediately before the restart**. A restart that re-executed the same image
/// satisfies the first two and fails the third.
///
/// That third anchor is the pid read here, not the pid of whoever asked. ccc
/// uses `origin_pid` because its scheduler *is* the bridge process, so the two
/// coincide; here the scheduler is usually a short-lived CLI whose pid the
/// unit never has, which would make the comparison trivially true and the
/// whole proof vacuous. Reading the unit's own MainPID first holds for any
/// caller.
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
    // The receipt is untrusted input even to its own worker: it names the unit
    // that is about to be restarted, and this process can restart anything the
    // manager will let it.
    if check_unit(&receipt.unit).is_err() {
        return finish(
            plan.data_dir,
            &mut receipt,
            Err(FailureCode::WorkerError),
            now(),
        );
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

    // Before the restart, so it describes the generation being replaced.
    // `None` — the unit is not running, or systemd would not say — is not a
    // reason to stop: anything that comes up afterwards is a replacement.
    let previous_pid = main_pid(&systemctl, plan.user_scope, &receipt.unit);
    receipt.previous_pid = previous_pid;

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
        RESTART_TIMEOUT,
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

/// What systemd says the unit's main process is, if it says anything usable.
fn main_pid(systemctl: &Path, user_scope: bool, unit: &str) -> Option<u32> {
    let pid = systemctl_output(
        systemctl,
        user_scope,
        &["show", "--property=MainPID", "--value", "--", unit],
    )?
    .trim()
    .parse::<u32>()
    .ok()?;
    // systemd reports 0 for a unit with no main process.
    (pid != 0).then_some(pid)
}

/// The pid that is serving, if it satisfies every part of the proof.
fn serving_pid(systemctl: &Path, plan: &WorkerPlan<'_>, receipt: &Receipt) -> Option<u32> {
    if !systemctl_ok(
        systemctl,
        plan.user_scope,
        &["is-active", "--quiet", "--", &receipt.unit],
        QUERY_TIMEOUT,
    ) {
        return None;
    }
    let main_pid = main_pid(systemctl, plan.user_scope, &receipt.unit)?;
    // The generation being replaced, read before the restart. Equal means the
    // unit re-executed the same process and nothing was replaced — #1527.
    if Some(main_pid) == receipt.previous_pid {
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
    // Only over its own request. A worker still running past
    // `ACTIVE_TTL_SECONDS` may find that a second scheduler has taken over and
    // written a fresh `prepared` receipt; writing this one on top would
    // destroy that request, and the new timer would then fire, see a foreign
    // id and restart nothing. ccc guards its failure path the same way.
    //
    // A worker that cannot write its result still exits with the right code:
    // dying silently would leave a `prepared` receipt blocking the next
    // request for its whole TTL.
    if matches!(
        read_receipt(data_dir),
        Ok(Some(ref current)) if current.request_id == receipt.request_id
    ) {
        let _ = write_receipt(data_dir, receipt);
    }
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

/// Bounds for the children this module spawns, matching ccc's.
///
/// Unbounded would be worse than it looks: a `systemctl restart` blocked on a
/// unit's `TimeoutStopSec` can outlive [`ACTIVE_TTL_SECONDS`], at which point a
/// second scheduler is entitled to take the request over and this worker is
/// writing results for a request that is no longer current.
const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// `systemd-run`'s own budget; ccc allows 8 seconds for the same D-Bus call.
const SCHEDULE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Wait for a child, killing it if it outstays the budget.
///
/// `None` means it was killed or could not be waited for — never "succeeded".
fn wait_bounded(child: &mut std::process::Child, budget: std::time::Duration) -> Option<bool> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn systemctl_ok(
    systemctl: &Path,
    user_scope: bool,
    args: &[&str],
    budget: std::time::Duration,
) -> bool {
    let spawned = systemctl_command(systemctl, user_scope, args)
        .stdin(std::process::Stdio::null())
        // Captured and dropped. Nothing systemd says goes into the receipt.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => wait_bounded(&mut child, budget).unwrap_or(false),
        Err(_) => false,
    }
}

/// A bounded read of a *small* systemd property.
///
/// Safe to wait before reading only because the value is a handful of bytes —
/// far inside the pipe buffer, so the child cannot block on writing it.
fn systemctl_output(systemctl: &Path, user_scope: bool, args: &[&str]) -> Option<String> {
    use std::io::Read;
    let mut child = systemctl_command(systemctl, user_scope, args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    if wait_bounded(&mut child, QUERY_TIMEOUT) != Some(true) {
        return None;
    }
    let stdout = child.stdout.take()?;
    let mut raw = Vec::new();
    stdout.take(4096).read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
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
            previous_pid: None,
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
        // The target is a private file this user owns, so every check except
        // `O_NOFOLLOW` passes on it. A symlink test pointed at a 0644 file
        // proves only that the mode check works.
        let elsewhere = dir.join("elsewhere.json");
        write_private(
            &elsewhere,
            &serde_json::to_vec(&receipt(State::Completed, 1)).unwrap(),
        )
        .unwrap();
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
        for mutate in [
            (|r: &mut Receipt| r.schema_version = 99) as fn(&mut Receipt),
            |r: &mut Receipt| r.schema = "danso.service.restart-handoff.v2".into(),
            |r: &mut Receipt| r.schema = "ccc.self-update.activation.v1".into(),
        ] {
            let mut foreign = receipt(State::Completed, 100);
            mutate(&mut foreign);
            write_receipt(&dir, &foreign).unwrap();
            assert_eq!(
                read_receipt(&dir),
                Err(HandoffError::InvalidReceipt),
                "both halves of the schema identify the document, not just the \
                 version"
            );
        }
    }

    #[test]
    fn an_oversized_receipt_is_refused() {
        let (_temp, dir) = data_dir();
        let mut huge = receipt(State::Prepared, 100);
        huge.unit = format!("{}.service", "a".repeat(MAX_RECEIPT_BYTES as usize));
        assert_eq!(
            write_receipt(&dir, &huge),
            Err(HandoffError::ReceiptTooLarge),
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
            assert_eq!(
                error,
                HandoffError::InvalidUnit,
                "a bad unit name is a bad unit name, not an unsafe directory: {bad:?}"
            );
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
    restart)
      [ -e "$here/restart_fails" ] && exit 1
      # A real restart changes MainPID. `after_pid` is what the unit reports
      # once it has restarted; a test that does not write one is modelling a
      # unit that re-executed the same process.
      [ -e "$here/after_pid" ] && cp "$here/after_pid" "$here/main_pid"
      exit 0 ;;
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
        publish_health_pids(path, pid, pid, at, state)
    }

    /// `service_pid` and `process.pid` separately: the proof reads both, and a
    /// fixture that sets them from one value lets either check be deleted for
    /// free.
    fn publish_health_pids(path: &Path, service_pid: u32, process_pid: u32, at: i64, state: &str) {
        let written = chrono::DateTime::from_timestamp(at, 0)
            .unwrap()
            .to_rfc3339();
        let document = serde_json::json!({
            "schema_version": 1,
            "started_at": written,
            "last_poll_at": written,
            "active_turn_count": 0,
            "queued_counts": {},
            "service_pid": service_pid,
            "process": {"pid": process_pid, "started_at": written, "mode": "run"},
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
        // The unit is serving 4242; the restart replaces it with 7777.
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        std::fs::write(temp.path().join("after_pid"), "7777\n").unwrap();
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
        assert_eq!(
            done.previous_pid,
            Some(4242),
            "the anchor is the unit's own pid before the restart, recorded so \
             an operator can see what was replaced"
        );
        assert_eq!(done.reason_code, None);
    }

    #[test]
    fn a_restart_that_re_executed_the_same_process_is_not_a_restart() {
        // ccc-node #1527: "the unit restarted" reported success while the
        // generation had not changed. The origin pid still serving means the
        // replacement never happened, however healthy it looks.
        let (temp, dir) = data_dir();
        // Production shape: the scheduler is a short-lived CLI, so `origin_pid`
        // is a pid the unit never had. Comparing against *that* is always true
        // and proves nothing — which is why the anchor is the unit's own
        // MainPID, read before the restart.
        let mut asked = receipt(State::Prepared, 1_000);
        asked.origin_pid = 999_001;
        write_receipt(&dir, &asked).unwrap();
        let systemctl = stub_systemctl(temp.path());
        // No `after_pid`: the unit reports the same MainPID after the restart
        // as before it, which is exactly the #1527 shape.
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
    fn both_pid_fields_in_the_health_document_must_agree() {
        // The proof reads `service_pid` *and* `process.pid`. A fixture that
        // sets them from one value lets either check be deleted for free, so
        // disagreement has to be a failure in its own right.
        for (service_pid, process_pid) in [(7777, 4242), (4242, 7777)] {
            let (temp, dir) = data_dir();
            write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
            let systemctl = stub_systemctl(temp.path());
            std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
            std::fs::write(temp.path().join("after_pid"), "7777\n").unwrap();
            let health = temp.path().join("health.json");
            publish_health_pids(&health, service_pid, process_pid, 1_001, "available");

            let code = run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_000, 30),
            );
            assert_eq!(
                code, 1,
                "a document whose two pids disagree ({service_pid}, {process_pid}) \
                 does not identify a serving process"
            );
        }
    }

    #[test]
    fn a_unit_with_no_main_process_is_not_a_replacement() {
        // systemd reports MainPID 0 for a unit with no main process. Reading
        // that as a pid would make "nothing is running" look like a restart.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "0\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 0, 1_001, "available");

        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_000, 30)
            ),
            1
        );
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().reason_code,
            Some(FailureCode::HealthTimeout)
        );
    }

    #[test]
    fn a_dead_unit_is_not_a_restart_however_good_the_health_looks() {
        // `is-active` is its own gate. Without it a health document left by a
        // process that has since died would satisfy every other clause.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("inactive"), b"").unwrap();
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        std::fs::write(temp.path().join("after_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 7777, 1_001, "available");

        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_000, 30)
            ),
            1,
            "everything but `is-active` says this restart worked"
        );
    }

    #[test]
    fn the_worker_arms_the_request_before_touching_systemd() {
        // `armed` is what makes a duplicate timer a no-op. If the transition
        // never lands, a second firing still sees `prepared` and restarts
        // again — and the guard that reads it is testing nothing.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        // The restart fails, so the worker stops right after arming.
        std::fs::write(temp.path().join("restart_fails"), b"").unwrap();
        let health = temp.path().join("health.json");
        run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            ticking(1_001, 1),
        );
        // It ends `failed`, but it passed through `armed`: a second worker
        // arriving now finds a state that is not `prepared` and does nothing.
        let after = read_receipt(&dir).unwrap().unwrap();
        assert_ne!(after.state, State::Prepared);
        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_002, 1)
            ),
            EXIT_WORKER_NOT_MINE
        );
    }

    #[test]
    fn a_worker_does_not_write_over_a_request_that_superseded_it() {
        // Past the TTL a second scheduler may take the request over. The first
        // worker, returning late, must not overwrite that fresh `prepared`
        // receipt: the new timer would then fire, see a foreign id, and
        // restart nothing while its receipt was gone.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");

        let mut mine = receipt(State::Armed, 1_000);
        let mut theirs = receipt(State::Prepared, 2_000);
        theirs.request_id = "ffffffffffffffff".to_string();
        write_receipt(&dir, &theirs).unwrap();

        finish(&dir, &mut mine, Err(FailureCode::HealthTimeout), 2_100);
        let current = read_receipt(&dir).unwrap().unwrap();
        assert_eq!(current.request_id, "ffffffffffffffff");
        assert_eq!(current.state, State::Prepared);
        let _ = systemctl;
        let _ = health;
    }

    #[test]
    fn health_slack_is_one_second_not_a_window() {
        // The slack exists because two clocks read the same wall clock, not to
        // admit documents from before the request. Widening it is how a
        // pre-restart document becomes proof.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        std::fs::write(temp.path().join("after_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        // One second early: still accepted.
        publish_health(&health, 7777, 999, "available");
        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_001, 1)
            ),
            0
        );

        // Two seconds early: not.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        std::fs::write(temp.path().join("after_pid"), "7777\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 7777, 998, "available");
        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_000, 30)
            ),
            1
        );
    }

    #[test]
    fn the_worker_refuses_a_receipt_naming_a_unit_it_did_not_validate() {
        // The receipt is the worker's only input and it names the unit about
        // to be restarted. `schedule` validates; so must the worker, because
        // nothing guarantees the same process wrote the file.
        let (temp, dir) = data_dir();
        let mut hostile = receipt(State::Prepared, 1_000);
        hostile.unit = "-p".to_string();
        write_receipt(&dir, &hostile).unwrap();
        let systemctl = stub_systemctl(temp.path());
        let health = temp.path().join("health.json");

        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_001, 1)
            ),
            1
        );
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().reason_code,
            Some(FailureCode::WorkerError),
            "and it must say it failed rather than quietly restarting nothing"
        );
    }

    #[test]
    fn a_relative_data_dir_is_refused_rather_than_scheduled() {
        // The transient unit's working directory is `/`, not the caller's
        // (measured on yukson 2026-09-17). A relative path would resolve
        // somewhere else entirely: the worker would find no receipt, exit
        // quietly, and the `prepared` receipt would block every later request
        // for its whole TTL while the caller had been told a restart was on
        // its way.
        let (_temp, _dir) = data_dir();
        let worker = std::env::current_exe().unwrap();
        let relative = PathBuf::from("state");
        let error = schedule(
            &Plan {
                data_dir: &relative,
                unit: "danso.service",
                worker: &worker,
                delay_seconds: DEFAULT_DELAY_SECONDS,
                user_scope: Some(true),
                systemd_run: None,
            },
            0,
        )
        .unwrap_err();
        assert_eq!(
            error,
            HandoffError::RelativeDataDir,
            "and it says which mistake it was: a missing directory reports \
             `unsafe_data_dir`, so sharing that code would make this check \
             impossible to tell apart from one"
        );
    }

    #[test]
    fn a_receipt_bigger_than_the_cap_is_refused_on_the_way_in_too() {
        // The write-side cap is not enough: the file is read by a different
        // process than wrote it, and an unbounded read of whatever is at that
        // name is how this module already killed itself once.
        let (_temp, dir) = data_dir();
        let path = receipt_path(&dir);
        write_private(&path, &vec![b'x'; (MAX_RECEIPT_BYTES + 1) as usize]).unwrap();
        assert_eq!(read_receipt(&dir), Err(HandoffError::ReceiptTooLarge));
    }

    #[test]
    fn a_receipt_in_a_directory_others_can_write_is_not_read() {
        // Every per-file check passes — it really is our own 0600 file. What
        // fails is the directory: somebody who can write it can rename an old
        // terminal receipt back into place and replay it.
        let (_temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Completed, 1)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(read_receipt(&dir), Err(HandoffError::UnsafeDataDir));
    }

    #[test]
    fn a_receipt_belonging_to_somebody_else_is_refused() {
        // The one clause a single-user test run cannot arrange on a real file.
        let ours = 1_000;
        assert!(is_private(true, 1, ours, 0o600, ours));
        assert!(
            !is_private(true, 1, ours + 1, 0o600, ours),
            "a private, single-linked, regular file that somebody else owns is \
             still somebody else's account of what happened to this restart"
        );
        assert!(!is_private(true, 1, 0, 0o600, ours), "root's, not ours");
        // And the clauses that are arranged on real files elsewhere, pinned
        // here too so each is independently load-bearing.
        assert!(
            !is_private(false, 1, ours, 0o600, ours),
            "not a regular file"
        );
        assert!(!is_private(true, 2, ours, 0o600, ours), "hard-linked");
        assert!(!is_private(true, 1, ours, 0o640, ours), "group-readable");
        assert!(!is_private(true, 1, ours, 0o601, ours), "other-executable");
    }

    #[test]
    fn a_hardlinked_receipt_is_refused() {
        // A second name for the same inode means somebody else can keep a
        // handle on it after we replace the one we know about.
        let (_temp, dir) = data_dir();
        let real = dir.join("kept.json");
        write_private(
            &real,
            &serde_json::to_vec(&receipt(State::Completed, 1)).unwrap(),
        )
        .unwrap();
        std::fs::hard_link(&real, receipt_path(&dir)).unwrap();
        assert_eq!(read_receipt(&dir), Err(HandoffError::UnsafeReceipt));
    }

    #[test]
    fn a_data_dir_that_is_not_a_directory_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let not_a_dir = temp.path().join("file");
        write_private(&not_a_dir, b"{}").unwrap();
        assert_eq!(read_receipt(&not_a_dir), Err(HandoffError::UnsafeDataDir));
        assert_eq!(
            write_receipt(&not_a_dir, &receipt(State::Prepared, 1)),
            Err(HandoffError::UnsafeDataDir)
        );
    }

    #[test]
    fn a_unit_that_lost_its_main_process_is_not_a_replacement() {
        // The unit was serving 4242 and comes back with no main process at
        // all. MainPID 0 is systemd saying "nothing"; reading it as a pid
        // makes an empty unit look like a successful restart.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("main_pid"), "4242\n").unwrap();
        std::fs::write(temp.path().join("after_pid"), "0\n").unwrap();
        let health = temp.path().join("health.json");
        publish_health(&health, 0, 1_001, "available");

        assert_eq!(
            run_worker(
                &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
                ticking(1_000, 30)
            ),
            1
        );
        assert_eq!(
            read_receipt(&dir).unwrap().unwrap().reason_code,
            Some(FailureCode::HealthTimeout)
        );
    }

    #[test]
    fn the_receipt_says_armed_while_systemctl_is_running() {
        // `armed` is what makes a duplicate timer a no-op, and the only moment
        // it is observable is during the restart. The stub copies the receipt
        // aside as it runs, which is the one way to see the transition landed
        // rather than inferring it from the state afterwards.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let path = temp.path().join("systemctl");
        let script = format!(
            "#!/bin/sh\nhere=\"$(dirname \"$0\")\"\nfor a in \"$@\"; do\n  \
             case \"$a\" in\n    restart) cp {receipt} \"$here/seen.json\"; exit 1 ;;\n  \
             esac\ndone\nexit 0\n",
            receipt = receipt_path(&dir).display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let health = temp.path().join("health.json");

        run_worker(
            &worker_plan(&dir, &health, &path, "0123456789abcdef"),
            ticking(1_001, 1),
        );
        let seen: Receipt =
            serde_json::from_slice(&std::fs::read(temp.path().join("seen.json")).unwrap()).unwrap();
        assert_eq!(
            seen.state,
            State::Armed,
            "the request must be marked taken before the restart, not after"
        );
        assert_eq!(
            seen.previous_pid, None,
            "and the pre-restart anchor is recorded in the same write"
        );
    }

    #[test]
    fn the_worker_gives_up_within_the_health_deadline() {
        // Otherwise the deadline is a number nothing reads: the loop would end
        // eventually either way, just much later, and no test would notice.
        let (temp, dir) = data_dir();
        write_receipt(&dir, &receipt(State::Prepared, 1_000)).unwrap();
        let systemctl = stub_systemctl(temp.path());
        std::fs::write(temp.path().join("inactive"), b"").unwrap();
        let health = temp.path().join("health.json");

        // Counted polls, not elapsed injected seconds. A bound written in
        // terms of the constant grows with it, so widening the deadline would
        // widen the assertion too and pin nothing. At 20 seconds a poll, a
        // one-minute deadline is a handful of rounds and a ten-minute one is
        // not.
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&polls);
        let mut clock = 1_000i64;
        run_worker(
            &worker_plan(&dir, &health, &systemctl, "0123456789abcdef"),
            move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let now = clock;
                clock += 20;
                now
            },
        );
        let polls = polls.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            polls <= 10,
            "{polls} clock reads before giving up; a one-minute deadline at 20 \
             seconds a poll is a few rounds, not dozens"
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
